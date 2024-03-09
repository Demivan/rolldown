use oxc::{
  allocator,
  ast::{
    ast::{self, SimpleAssignmentTarget},
    VisitMut,
  },
  span::Span,
};
use rolldown_common::{ExportsKind, ModuleId, SymbolRef, WrapKind};
use rolldown_oxc_utils::{Dummy, ExpressionExt, IntoIn, StatementExt, TakeIn};

use super::Finalizer;

impl<'ast, 'me: 'ast> Finalizer<'me, 'ast> {
  fn visit_top_level_statement_mut(&mut self, stmt: &mut ast::Statement<'ast>) {
    self.visit_statement(stmt);
  }
}

impl<'ast, 'me: 'ast> VisitMut<'ast> for Finalizer<'me, 'ast> {
  #[allow(clippy::too_many_lines, clippy::match_same_arms)]
  fn visit_program(&mut self, program: &mut ast::Program<'ast>) {
    let old_body = program.body.take_in(self.alloc);
    let is_namespace_referenced = matches!(self.ctx.module.exports_kind, ExportsKind::Esm)
      && self.ctx.module.stmt_infos[0].is_included;

    if is_namespace_referenced {
      program.body.extend(self.generate_namespace_variable_declaration());
    }

    let mut stmt_infos = self.ctx.module.stmt_infos.iter();
    stmt_infos.next();

    old_body.into_iter().enumerate().zip(stmt_infos).for_each(
      |((_top_stmt_idx, mut top_stmt), stmt_info)| {
        debug_assert!(matches!(stmt_info.stmt_idx, Some(_top_stmt_idx)));
        if !stmt_info.is_included {
          return;
        }

        if let Some(import_decl) = top_stmt.as_import_declaration() {
          let rec_id = self.ctx.module.imports[&import_decl.span];
          if self.should_remove_import_export_stmt(&mut top_stmt, rec_id) {
            return;
          }
        } else if let Some(export_all_decl) = top_stmt.as_export_all_declaration() {
          let rec_id = self.ctx.module.imports[&export_all_decl.span];
          // "export * as ns from 'path'"
          if let Some(_alias) = &export_all_decl.exported {
            if self.should_remove_import_export_stmt(&mut top_stmt, rec_id) {
              return;
            }
          } else {
            // "export * from 'path'"
            let rec = &self.ctx.module.import_records[rec_id];
            let ModuleId::Normal(importee_id) = rec.resolved_module else {
              // TODO: handle re-exporting all from external module
              return;
            };
            let importee_linking_info = &self.ctx.linking_infos[importee_id];
            let importee = &self.ctx.modules[importee_id];
            if matches!(importee_linking_info.wrap_kind, WrapKind::Esm) {
              let wrapper_ref_name =
                self.canonical_name_for(importee_linking_info.wrapper_ref.unwrap());
              program.body.push(self.snippet.call_expr_stmt(wrapper_ref_name.to_oxc_atom()));
            }

            match importee.exports_kind {
              ExportsKind::Esm => {
                if importee_linking_info.has_dynamic_exports {
                  let re_export_fn_name = self.canonical_name_for_runtime("__reExport");
                  let importer_namespace_name =
                    self.canonical_name_for(self.ctx.module.namespace_symbol);
                  // __reExport(exports, otherExports)
                  let importee_namespace_name = self.canonical_name_for(importee.namespace_symbol);
                  program.body.push(
                    self
                      .snippet
                      .call_expr_with_2arg_expr(
                        re_export_fn_name.to_oxc_atom(),
                        importer_namespace_name.to_oxc_atom(),
                        importee_namespace_name.to_oxc_atom(),
                      )
                      .into_in(self.alloc),
                  );
                }
              }
              ExportsKind::CommonJs => {
                let re_export_fn_name = self.canonical_name_for_runtime("__reExport");
                let importer_namespace_name =
                  self.canonical_name_for(self.ctx.module.namespace_symbol);
                // __reExport(exports, __toESM(require_xxxx()))
                let to_esm_fn_name = self.canonical_name_for_runtime("__toESM");
                let importee_wrapper_ref_name =
                  self.canonical_name_for(importee_linking_info.wrapper_ref.unwrap());
                program.body.push(
                  self
                    .snippet
                    .call_expr_with_2arg_expr_expr(
                      re_export_fn_name.to_oxc_atom(),
                      self.snippet.id_ref_expr(importer_namespace_name.to_oxc_atom()),
                      self.snippet.call_expr_with_arg_expr_expr(
                        to_esm_fn_name.to_oxc_atom(),
                        self.snippet.call_expr_expr(importee_wrapper_ref_name.to_oxc_atom()),
                      ),
                    )
                    .into_in(self.alloc),
                );
              }
              ExportsKind::None => {}
            }
            return;
          }
        } else if let Some(default_decl) = top_stmt.as_export_default_declaration_mut() {
          match &mut default_decl.declaration {
            ast::ExportDefaultDeclarationKind::Expression(expr) => {
              // "export default foo;" => "var default = foo;"
              let canonical_name_for_default_export_ref =
                self.canonical_name_for(self.ctx.module.default_export_ref);
              top_stmt = self.snippet.var_decl_stmt(
                canonical_name_for_default_export_ref.to_oxc_atom(),
                expr.take_in(self.alloc),
              );
            }
            ast::ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
              // "export default function() {}" => "function default() {}"
              // "export default function foo() {}" => "function foo() {}"
              if func.id.is_none() {
                let canonical_name_for_default_export_ref =
                  self.canonical_name_for(self.ctx.module.default_export_ref);
                func.id =
                  Some(self.snippet.id(canonical_name_for_default_export_ref.to_oxc_atom()));
              }
              top_stmt = ast::Statement::Declaration(ast::Declaration::FunctionDeclaration(
                func.take_in(self.alloc),
              ));
            }
            ast::ExportDefaultDeclarationKind::ClassDeclaration(class) => {
              // "export default class {}" => "class default {}"
              // "export default class Foo {}" => "class Foo {}"
              if class.id.is_none() {
                let canonical_name_for_default_export_ref =
                  self.canonical_name_for(self.ctx.module.default_export_ref);
                class.id =
                  Some(self.snippet.id(canonical_name_for_default_export_ref.to_oxc_atom()));
              }
              top_stmt = ast::Statement::Declaration(ast::Declaration::ClassDeclaration(
                class.take_in(self.alloc),
              ));
            }
            _ => {}
          }
        } else if let Some(named_decl) = top_stmt.as_export_named_declaration_mut() {
          if named_decl.source.is_none() {
            if let Some(decl) = &mut named_decl.declaration {
              // `export var foo = 1` => `var foo = 1`
              // `export function foo() {}` => `function foo() {}`
              // `export class Foo {}` => `class Foo {}`
              top_stmt = ast::Statement::Declaration(decl.take_in(self.alloc));
            } else {
              // `export { foo }`
              // Remove this statement by ignoring it
              return;
            }
          } else {
            // `export { foo } from 'path'`
            let rec_id = self.ctx.module.imports[&named_decl.span];
            if self.should_remove_import_export_stmt(&mut top_stmt, rec_id) {
              return;
            }
          }
        }

        program.body.push(top_stmt);
      },
    );

    // visit children
    for directive in program.directives.iter_mut() {
      self.visit_directive(directive);
    }
    for stmt in program.body.iter_mut() {
      self.visit_top_level_statement_mut(stmt);
    }

    // check if we need to add wrapper
    let needs_wrapper = self
      .ctx
      .linking_info
      .wrapper_stmt_info
      .map(|idx| self.ctx.module.stmt_infos[idx].is_included)
      .unwrap_or_default();

    if needs_wrapper {
      match self.ctx.linking_info.wrap_kind {
        WrapKind::Cjs => {
          let wrap_ref_name = self.canonical_name_for(self.ctx.linking_info.wrapper_ref.unwrap());
          let commonjs_ref_name = self.canonical_name_for_runtime("__commonJSMin");
          let old_body = program.body.take_in(self.alloc);

          program.body.push(self.snippet.commonjs_wrapper_stmt(
            wrap_ref_name.to_oxc_atom(),
            commonjs_ref_name.to_oxc_atom(),
            old_body,
          ));
        }
        WrapKind::Esm => {
          let wrap_ref_name = self.canonical_name_for(self.ctx.linking_info.wrapper_ref.unwrap());
          let esm_ref_name = self.canonical_name_for_runtime("__esmMin");
          let old_body = program.body.take_in(self.alloc);

          let mut fn_stmts = allocator::Vec::new_in(self.alloc);
          let mut hoisted_names = vec![];
          let mut stmts_inside_closure = allocator::Vec::new_in(self.alloc);

          // Hoist all top-level "var" and "function" declarations out of the closure
          old_body.into_iter().for_each(|mut stmt| match &mut stmt {
            ast::Statement::Declaration(decl) => match decl {
              ast::Declaration::VariableDeclaration(_) | ast::Declaration::ClassDeclaration(_) => {
                if let Some(converted) = self.convert_decl_to_assignment(decl, &mut hoisted_names) {
                  stmts_inside_closure.push(converted);
                }
              }
              ast::Declaration::FunctionDeclaration(_) => {
                fn_stmts.push(stmt);
              }
              ast::Declaration::UsingDeclaration(_) => unimplemented!(),
              _ => {}
            },
            ast::Statement::ModuleDeclaration(_) => unreachable!(
              "At this point, all module declarations should have been removed or transformed"
            ),
            _ => {
              stmts_inside_closure.push(stmt);
            }
          });
          program.body.extend(fn_stmts);
          if !hoisted_names.is_empty() {
            let mut declarators = allocator::Vec::new_in(self.alloc);
            declarators.reserve_exact(hoisted_names.len());
            hoisted_names.into_iter().for_each(|var_name| {
              declarators.push(ast::VariableDeclarator {
                id: ast::BindingPattern {
                  kind: ast::BindingPatternKind::BindingIdentifier(
                    self.snippet.id(var_name).into_in(self.alloc),
                  ),
                  ..Dummy::dummy(self.alloc)
                },
                kind: ast::VariableDeclarationKind::Var,
                ..Dummy::dummy(self.alloc)
              });
            });
            program.body.push(ast::Statement::Declaration(ast::Declaration::VariableDeclaration(
              ast::VariableDeclaration {
                declarations: declarators,
                kind: ast::VariableDeclarationKind::Var,
                ..Dummy::dummy(self.alloc)
              }
              .into_in(self.alloc),
            )));
          }
          program.body.push(self.snippet.esm_wrapper_stmt(
            wrap_ref_name.to_oxc_atom(),
            esm_ref_name.to_oxc_atom(),
            stmts_inside_closure,
          ));
        }
        WrapKind::None => {}
      }
    }
  }

  fn visit_binding_identifier(&mut self, ident: &mut ast::BindingIdentifier) {
    if let Some(symbol_id) = ident.symbol_id.get() {
      let symbol_ref: SymbolRef = (self.ctx.id, symbol_id).into();

      let canonical_ref = self.ctx.symbols.par_canonical_ref_for(symbol_ref);
      let symbol = self.ctx.symbols.get(canonical_ref);
      assert!(symbol.namespace_alias.is_none());
      let canonical_name = self.canonical_name_for(symbol_ref);
      if ident.name != canonical_name.as_str() {
        ident.name = canonical_name.to_oxc_atom();
      }
      ident.symbol_id.get_mut().take();
    } else {
      // Some `BindingIdentifier`s constructed by bundler don't have `SymbolId` and we just ignore them.
    }
  }

  fn visit_identifier_reference(&mut self, ident: &mut ast::IdentifierReference) {
    // This ensure all `IdentifierReference`s are processed
    debug_assert!(
      self.is_global_identifier_reference(ident) || ident.reference_id.get().is_none(),
      "{} doesn't get processed in {}",
      ident.name,
      self.ctx.module.repr_name
    );
  }

  fn visit_call_expression(&mut self, expr: &mut ast::CallExpression<'ast>) {
    self.try_rewrite_identifier_reference_expr(&mut expr.callee, true);

    // visit children
    for arg in expr.arguments.iter_mut() {
      self.visit_argument(arg);
    }
    self.visit_expression(&mut expr.callee);
    if let Some(parameters) = &mut expr.type_parameters {
      self.visit_ts_type_parameter_instantiation(parameters);
    }
  }

  fn visit_class(&mut self, class: &mut ast::Class<'ast>) {
    // Visit children without `visit_binding_identifier` as we don't want to rename class and cause runtime difference

    for decorator in class.decorators.iter_mut() {
      self.visit_decorator(decorator);
    }

    if let Some(parameters) = &mut class.type_parameters {
      self.visit_ts_type_parameter_declaration(parameters);
    }

    if let Some(super_class) = &mut class.super_class {
      self.visit_class_heritage(super_class);
    }
    if let Some(super_parameters) = &mut class.super_type_parameters {
      self.visit_ts_type_parameter_instantiation(super_parameters);
    }
    self.visit_class_body(&mut class.body);
  }

  fn visit_variable_declarator(&mut self, declarator: &mut ast::VariableDeclarator<'ast>) {
    // If we rename a variable declaration, we should preserve the class name
    // `var a = class { }` should become `var a$1 = class a { }`

    // Preserve the class name if it's a class declaration
    let mut old_name = None;

    if let (
      ast::BindingPatternKind::BindingIdentifier(ident),
      Some(ast::Expression::ClassExpression(_)),
    ) = (&mut declarator.id.kind, &declarator.init)
    {
      old_name = Some(ident.name.clone());
    }

    // Visit binding pattern
    self.visit_binding_pattern(&mut declarator.id);

    // Put old name into the declarator if it's a class declaration
    if let (
      Some(old_name),
      ast::BindingPatternKind::BindingIdentifier(ident),
      Some(ast::Expression::ClassExpression(class)),
    ) = (old_name, &mut declarator.id.kind, &mut declarator.init)
    {
      if ident.name != old_name {
        class.id = Some(self.snippet.id(old_name));
      }
    }

    // Visit children
    if let Some(init) = &mut declarator.init {
      self.visit_expression(init);
    }
  }

  fn visit_declaration_match(&mut self, decl: &mut ast::Declaration<'ast>) {
    match decl {
      ast::Declaration::VariableDeclaration(decl) => self.visit_variable_declaration(decl),
      ast::Declaration::FunctionDeclaration(func) => self.visit_function(func, None),
      ast::Declaration::ClassDeclaration(class) => {
        // Check if we need to rename the class
        // If we need to rename the class, we should convert the class declaration to a variable declaration
        // `class A { }` => `var A$1 = class A { }`
        if let Some(ident) = &mut class.id {
          if let Some(symbol_id) = ident.symbol_id.get() {
            let canonical_name = self.canonical_name_for((self.ctx.id, symbol_id).into());
            if ident.name != canonical_name.as_str() {
              let class_expr = ast::Expression::ClassExpression(class.take_in(self.alloc));
              let var_decl = self.snippet.var_decl(canonical_name.to_oxc_atom(), class_expr);

              *decl = var_decl;

              return;
            }
          }
        }

        self.visit_class(class);
      }
      ast::Declaration::UsingDeclaration(decl) => self.visit_using_declaration(decl),
      ast::Declaration::TSModuleDeclaration(module) => {
        self.visit_ts_module_declaration(module);
      }
      ast::Declaration::TSTypeAliasDeclaration(decl) => {
        self.visit_ts_type_alias_declaration(decl);
      }
      ast::Declaration::TSEnumDeclaration(decl) => self.visit_enum(decl),
      ast::Declaration::TSImportEqualsDeclaration(decl) => {
        self.visit_ts_import_equals_declaration(decl);
      }
      ast::Declaration::TSInterfaceDeclaration(decl) => {
        self.visit_ts_interface_declaration(decl);
      }
    }
  }

  #[allow(clippy::collapsible_else_if)]
  fn visit_expression(&mut self, expr: &mut ast::Expression<'ast>) {
    if let Some(call_expr) = expr.as_call_expression() {
      // Rewrite `require(...)` to `require_xxx(...)` or `(init_xxx(), __toCommonJS(xxx_exports))`
      if let ast::Expression::Identifier(callee) = &call_expr.callee {
        if callee.name == "require" && self.is_global_identifier_reference(callee) {
          let rec_id = self.ctx.module.imports[&call_expr.span];
          let rec = &self.ctx.module.import_records[rec_id];
          if let ModuleId::Normal(importee_id) = rec.resolved_module {
            let importee = &self.ctx.modules[importee_id];
            let importee_linking_info = &self.ctx.linking_infos[importee.id];
            let wrap_ref_name = self.canonical_name_for(importee_linking_info.wrapper_ref.unwrap());
            if matches!(importee.exports_kind, ExportsKind::CommonJs) {
              *expr = self.snippet.call_expr_expr(wrap_ref_name.to_oxc_atom());
            } else {
              let ns_name = self.canonical_name_for(importee.namespace_symbol);
              let to_commonjs_ref_name = self.canonical_name_for_runtime("__toCommonJS");
              *expr = self.snippet.seq2_in_paren_expr(
                self.snippet.call_expr_expr(wrap_ref_name.to_oxc_atom()),
                self.snippet.call_expr_with_arg_expr(
                  to_commonjs_ref_name.to_oxc_atom(),
                  ns_name.to_oxc_atom(),
                ),
              );
            }
          }
        }
      }
    }

    self.try_rewrite_identifier_reference_expr(expr, false);

    // visit children
    self.visit_expression_match(expr);
  }

  fn visit_object_property(&mut self, prop: &mut ast::ObjectProperty<'ast>) {
    // Ensure `{ a }` would be rewritten to `{ a: a$1 }` instead of `{ a$1 }`
    match &mut prop.value {
      ast::Expression::Identifier(id_ref) if prop.shorthand => {
        if let Some(expr) = self.generate_finalized_expr_for_reference(id_ref, false) {
          prop.value = expr;
          prop.shorthand = false;
        } else {
          id_ref.reference_id.get_mut().take();
        }
      }
      _ => {}
    }

    // visit children
    self.visit_property_key(&mut prop.key);
    self.visit_expression(&mut prop.value);
    if let Some(init) = &mut prop.init {
      self.visit_expression(init);
    }
  }

  fn visit_object_pattern(&mut self, pat: &mut ast::ObjectPattern<'ast>) {
    for prop in pat.properties.iter_mut() {
      match &mut prop.value.kind {
        // Ensure `const { a } = ...;` will be rewritten to `const { a: a$1 } = ...` instead of `const { a$1 } = ...`
        // Ensure `function foo({ a }) {}` will be rewritten to `function foo({ a: a$1 }) {}` instead of `function foo({ a$1 }) {}`
        ast::BindingPatternKind::BindingIdentifier(ident) if prop.shorthand => {
          if let Some(symbol_id) = ident.symbol_id.get() {
            let canonical_name = self.canonical_name_for((self.ctx.id, symbol_id).into());
            if ident.name != canonical_name.as_str() {
              ident.name = canonical_name.to_oxc_atom();
              prop.shorthand = false;
            }
            ident.symbol_id.get_mut().take();
          }
        }
        // Ensure `const { a = 1 } = ...;` will be rewritten to `const { a: a$1 = 1 } = ...` instead of `const { a$1 = 1 } = ...`
        // Ensure `function foo({ a = 1 }) {}` will be rewritten to `function foo({ a: a$1 = 1 }) {}` instead of `function foo({ a$1 = 1 }) {}`
        ast::BindingPatternKind::AssignmentPattern(assign_pat)
          if prop.shorthand
            && matches!(assign_pat.left.kind, ast::BindingPatternKind::BindingIdentifier(_)) =>
        {
          let ast::BindingPatternKind::BindingIdentifier(ident) = &mut assign_pat.left.kind else {
            unreachable!()
          };
          if let Some(symbol_id) = ident.symbol_id.get() {
            let canonical_name = self.canonical_name_for((self.ctx.id, symbol_id).into());
            if ident.name != canonical_name.as_str() {
              ident.name = canonical_name.to_oxc_atom();
              prop.shorthand = false;
            }
            ident.symbol_id.get_mut().take();
          }
        }
        _ => {
          // For other patterns:
          // - `const [a] = ...` or `function foo([a]) {}`
          // - `const { a: b } = ...` or `function foo({ a: b }) {}`
          // - `const { a: b = 1 } = ...` or `function foo({ a: b = 1 }) {}`
          // They could keep correct semantics after renaming, so we don't need to do anything special.
        }
      }
    }

    // visit children
    for prop in pat.properties.iter_mut() {
      self.visit_binding_property(prop);
    }
    if let Some(rest) = &mut pat.rest {
      self.visit_rest_element(rest);
    }
  }

  fn visit_import_expression(&mut self, expr: &mut ast::ImportExpression<'ast>) {
    // Make sure the import expression is in correct form. If it's not, we should leave it as it is.
    match &mut expr.source {
      ast::Expression::StringLiteral(str) if expr.arguments.len() == 0 => {
        let rec_id = self.ctx.module.imports[&expr.span];
        let rec = &self.ctx.module.import_records[rec_id];
        let importee_id = rec.resolved_module;
        match importee_id {
          ModuleId::Normal(importee_id) => {
            let chunk_id = self.ctx.chunk_graph.module_to_chunk[importee_id]
              .expect("Normal module should belong to a chunk");
            let chunk = &self.ctx.chunk_graph.chunks[chunk_id];
            str.value = format!("./{}", chunk.file_name.as_ref().unwrap()).into();
          }
          ModuleId::External(_) => {
            // external module doesn't belong to any chunk, just keep this as it is
          }
        }
      }
      _ => {}
    }

    // visit children
    self.visit_expression(&mut expr.source);
    for arg in expr.arguments.iter_mut() {
      self.visit_expression(arg);
    }
  }

  fn visit_assignment_target_property(
    &mut self,
    property: &mut ast::AssignmentTargetProperty<'ast>,
  ) {
    if let ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(prop) = property {
      if let Some(target) =
        self.generate_finalized_simple_assignment_target_for_reference(&prop.binding)
      {
        *property = ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(
          ast::AssignmentTargetPropertyProperty {
            name: ast::PropertyKey::Identifier(
              self.snippet.id_name(prop.binding.name.clone()).into_in(self.alloc),
            ),
            binding: if let Some(init) = prop.init.take() {
              ast::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(
                ast::AssignmentTargetWithDefault {
                  binding: ast::AssignmentTarget::SimpleAssignmentTarget(target),
                  init,
                  span: Span::default(),
                }
                .into_in(self.alloc),
              )
            } else {
              ast::AssignmentTargetMaybeDefault::AssignmentTarget(
                ast::AssignmentTarget::SimpleAssignmentTarget(target),
              )
            },
            span: Span::default(),
          }
          .into_in(self.alloc),
        );
      } else {
        prop.binding.reference_id.get_mut().take();
      }
    }

    // visit children
    match property {
      ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(ident) => {
        self.visit_assignment_target_property_identifier(ident);
      }
      ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(prop) => {
        self.visit_assignment_target_property_property(prop);
      }
    }
  }

  fn visit_simple_assignment_target(&mut self, target: &mut SimpleAssignmentTarget<'ast>) {
    self.rewrite_simple_assignment_target(target);

    // visit children
    match target {
      SimpleAssignmentTarget::AssignmentTargetIdentifier(ident) => {
        self.visit_identifier_reference(ident);
      }
      SimpleAssignmentTarget::MemberAssignmentTarget(expr) => {
        self.visit_member_expression(expr);
      }
      SimpleAssignmentTarget::TSAsExpression(expr) => {
        self.visit_expression(&mut expr.expression);
      }
      SimpleAssignmentTarget::TSSatisfiesExpression(expr) => {
        self.visit_expression(&mut expr.expression);
      }
      SimpleAssignmentTarget::TSNonNullExpression(expr) => {
        self.visit_expression(&mut expr.expression);
      }
      SimpleAssignmentTarget::TSTypeAssertion(expr) => {
        self.visit_expression(&mut expr.expression);
      }
    }
  }
}
