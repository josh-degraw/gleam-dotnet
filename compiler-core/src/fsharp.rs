#[cfg(test)]
mod tests;

use crate::{
    analyse::Inferred,
    ast::*,
    docvec,
    pretty::*,
    type_::{
        Deprecation, FieldMap, PatternConstructor, Type, TypeVar, ValueConstructor,
        ValueConstructorVariant,
    },
};
use ecow::EcoString;
use itertools::Itertools;
use regex::{Captures, Regex};
use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
    sync::{Arc, OnceLock},
};

const INDENT: isize = 4;
pub const FSHARP_PRELUDE: &str = include_str!("./fsharp/prelude.fs");

#[derive(Debug)]
pub struct Generator<'a> {
    pub external_files: HashSet<&'a EcoString>,
    module: &'a TypedModule,
}

mod prelude_functions {
    /// This is used directly in pattern matching
    pub const STRING_PATTERN_PREFIX: &str = "Gleam__codegen__prefix";

    // TODO: Is it worth it to have two separate functions here?
    // Probably not

    /// This is used directly in pattern matching
    pub const STRING_PATTERN_PARTS: &str = "Gleam_codegen_string_parts";
}

fn is_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "asr"
            | "land"
            | "lor"
            | "lsl"
            | "lsr"
            | "lxor"
            | "mod"
            | "sig"
            | "break"
            | "checked"
            | "component"
            | "const"
            | "constraint"
            | "continue"
            | "event"
            | "external"
            | "include"
            | "mixin"
            | "parallel"
            | "process"
            | "protected"
            | "pure"
            | "sealed"
            | "tailcall"
            | "trait"
            | "virtual"

            // Keywords
            | "abstract"
            | "and"
            | "as"
            | "assert"
            | "base"
            | "begin"
            | "class"
            | "default"
            | "delegate"
            | "do"
            | "done"
            | "downcast"
            | "downto"
            | "elif"
            | "else"
            | "end"
            | "exception"
            | "extern"
            | "false"
            | "finally"
            | "fixed"
            | "for"
            | "fun"
            | "function"
            | "global"
            | "if"
            | "in"
            | "inherit"
            | "inline"
            | "interface"
            | "internal"
            | "lazy"
            | "let"
            | "match"
            | "member"
            | "module"
            | "mutable"
            | "namespace"
            | "new"
            | "not"
            | "null"
            | "of"
            | "open"
            | "or"
            | "override"
            | "private"
            | "public"
            | "rec"
            | "return"
            | "select"
            | "static"
            | "struct"
            | "then"
            | "to"
            | "true"
            | "try"
            | "type"
            | "upcast"
            | "use"
            | "val"
            | "void"
            | "when"
            | "while"
            | "with"
            | "yield"
    )
}

fn unicode_escape_sequence_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"(\\+)(u)\{(\S+)\}"#)
            .expect("Unicode escape sequence regex cannot be constructed")
    })
}

impl<'a> Generator<'a> {
    pub fn new(module: &'a TypedModule) -> Self {
        Self {
            external_files: HashSet::new(),
            module,
        }
    }

    pub fn render(&mut self) -> super::Result<String> {
        let document = join(
            vec![self.module_declaration(), self.module_contents()],
            line(),
        );
        Ok(document.to_pretty_string(120))
    }

    /// Update the currently referenced module and render it
    pub fn render_module(&mut self, new_module: &'a TypedModule) -> super::Result<String> {
        self.module = new_module;
        self.render()
    }

    fn module_declaration(&mut self) -> Document<'a> {
        //println!("module: {:#?}", module);
        // Use module rec to not need to worry about initialization order
        "module rec "
            .to_doc()
            .append(self.santitize_name(&self.module.name))
    }

    fn module_contents(&mut self) -> Document<'a> {
        join(
            self.module
                .definitions
                .iter()
                .map(|def| match def {
                    Definition::CustomType(t) => self.custom_type(t),
                    Definition::TypeAlias(t) => self.type_alias(t),
                    Definition::ModuleConstant(c) => self.module_constant(c),
                    Definition::Function(f) => self.function(f),
                    Definition::Import(_) => docvec!["// TODO: Implement imports"],
                })
                .collect::<Vec<Document<'a>>>(),
            line(),
        )
    }

    fn map_publicity(&self, publicity: Publicity) -> Document<'a> {
        match publicity {
            Publicity::Public => nil(),
            Publicity::Internal { .. } => "internal ".to_doc(),
            Publicity::Private => "private ".to_doc(),
        }
    }

    fn is_fsharp_union_type(&self, t: &'a CustomType<Arc<Type>>) -> bool {
        // If there is more than one type constructor, it must be an F# union type
        if t.constructors.len() != 1 {
            true
        } else {
            assert!(t.constructors.len() == 1);
            // Might be an F# record type, but only if the constructor name
            // matches the type name and all constructor arguments have a label
            let c = t
                .constructors
                .first()
                .expect("Type must have a constructor");

            // Single constructor must match the type name and all arguments must have labels, otherwise
            // it must be an F# union type
            if c.name != t.name
                || c.arguments.is_empty()
                || c.arguments.iter().any(|a| a.label.is_none())
            {
                return true;
            }
            false
        }
    }

    /// Should convert a single-constructor custom type to an F# record
    /// gleam type:
    ///
    /// ```gleam
    /// pub type Person { Person(name: String, age: Int) }
    /// ```
    ///
    /// f# type:
    ///
    /// ```fsharp
    /// type Person = { name: string; age: int }
    fn custom_type(&self, type_: &'a CustomType<Arc<Type>>) -> Document<'a> {
        if self.is_fsharp_union_type(type_) {
            self.discriminated_union(type_)
        } else {
            self.record_type(type_)
        }
    }

    fn record_type(&self, type_: &'a CustomType<Arc<Type>>) -> Document<'a> {
        let constructor = type_
            .constructors
            .first()
            .expect("Single constructor should exist");

        let name = &type_.name;
        let fields = constructor
            .arguments
            .iter()
            .map(|r| {
                let type_doc = self.type_to_fsharp(&r.type_);
                match &r.label {
                    Some((_, ref label)) => docvec![self.santitize_name(label), ": ", type_doc],
                    None => type_doc,
                }
            })
            .collect::<Vec<Document<'a>>>();

        let opacity = if type_.opaque {
            line()
                .nest(INDENT)
                .append("private ".to_doc())
                .append(line().nest(INDENT * 2))
        } else {
            nil()
        };
        docvec![
            "type ",
            self.map_publicity(type_.publicity),
            name,
            self.type_params(type_),
            " = ",
            opacity,
            join(fields, "; ".to_doc())
                .surround("{ ", " }")
                .group()
                .nest(INDENT),
        ]
    }

    fn type_params(&self, t: &CustomType<Arc<Type>>) -> Document<'a> {
        let type_params = join(
            t.typed_parameters.iter().map(|tp| self.type_to_fsharp(tp)),
            ", ".to_doc(),
        )
        .surround("<", ">");

        if !t.typed_parameters.is_empty() {
            type_params
        } else {
            nil()
        }
    }

    fn discriminated_union(&self, t: &'a CustomType<Arc<Type>>) -> Document<'a> {
        // need to track which named fields have the same name and position and generate method accessors for them

        let mut named_fields = HashMap::new();

        let mut max_indices = vec![];

        let type_name = &t.name;
        // Need to sort the constructors before mapping to ensure repeatable output
        let constructors = t
            .constructors
            .iter()
            .sorted_by(|a, b| Ord::cmp(&a.name, &b.name))
            .map(|constructor| {
                let constructor_name = constructor.name.clone();
                let constructor_name_doc = constructor_name.to_doc();

                let mut field_index = 0;
                let fields = constructor
                    .arguments
                    .iter()
                    .map(|r| {
                        let type_ = self.type_to_fsharp(&r.type_);
                        // Need to wrap in parens if it's a function type
                        let type_doc = if r.type_.is_fun() {
                            type_.surround("(", ")")
                        } else {
                            type_
                        };

                        let res = match &r.label {
                            Some((_, ref arg_name)) => {
                                named_fields
                                    .entry(arg_name.clone())
                                    .or_insert(Vec::new())
                                    .push((field_index, r.type_.clone(), constructor));

                                docvec![arg_name, ": ", type_doc]
                            }
                            None => type_doc,
                        };

                        field_index += 1;
                        res
                    })
                    .collect::<Vec<Document<'a>>>();

                if !constructor.arguments.is_empty() {
                    max_indices.push(constructor.arguments.len() - 1);
                }
                if fields.is_empty() {
                    constructor_name_doc
                } else {
                    docvec![constructor_name_doc, " of ", join(fields, " * ".to_doc())]
                }
            })
            .collect::<Vec<Document<'a>>>();

        let member_declarations = named_fields
            .iter()
            .sorted_by(|a, b| Ord::cmp(&a.0, &b.0))
            .filter_map(|(label, type_loc_list)| {
                if type_loc_list.len() == t.constructors.len() {
                    let (first_index, first_type, _) = type_loc_list
                        .first()
                        .expect("Type loc list should have elements");

                    let meets_requirements = type_loc_list
                        .iter()
                        .all(|(index, type_, _)| index == first_index && type_ == first_type);

                    if meets_requirements {
                        let cases = type_loc_list
                            .iter()
                            .sorted_by(|a, b| Ord::cmp(&a.2.name, &b.2.name))
                            .map(|(index, _, constructor)| {
                                let max_index = constructor.arguments.len() - 1;

                                let mut discards = (0..=max_index)
                                    .map(|_| "_".to_doc())
                                    .collect::<Vec<Document<'a>>>();

                                *discards.get_mut(*index).expect("Index out of bounds") =
                                    label.to_doc();

                                let discards_doc =
                                    join(discards, ", ".to_doc()).surround("(", ")").group();

                                docvec![
                                    "| ",
                                    type_name.clone(),
                                    ".",
                                    constructor.name.clone().to_doc(),
                                    " ",
                                    discards_doc,
                                    " -> ",
                                    label,
                                ]
                            })
                            .collect::<Vec<_>>();

                        return Some(
                            docvec![
                                "member this.",
                                label,
                                " = ",
                                line(),
                                "match this with",
                                line(),
                                join(cases, line()),
                            ]
                            .group()
                            .nest(INDENT),
                        );
                    }
                }
                None
            });

        let member_declarations_doc = join(member_declarations, line()).nest(INDENT);

        let opacity = if t.opaque {
            line()
                .nest(INDENT)
                .append("private ".to_doc())
                .append(line().nest(INDENT))
        } else {
            line()
        };

        let case_indent = if t.opaque { INDENT } else { 0 };

        docvec![
            "type ",
            self.map_publicity(t.publicity),
            type_name,
            self.type_params(t),
            " =",
            opacity,
            join(
                constructors.into_iter().map(|c| docvec!["| ".to_doc(), c]),
                line()
            )
            .group()
            .nest(case_indent),
            line().nest(INDENT),
            member_declarations_doc
        ]
    }

    fn type_alias(&self, t: &'a TypeAlias<Arc<Type>>) -> Document<'a> {
        docvec![
            "type ",
            self.map_publicity(t.publicity),
            t.alias.clone(),
            " = ",
            self.type_to_fsharp(&t.type_)
        ]
    }

    fn function(&mut self, f: &'a TypedFunction) -> Document<'a> {
        let Function {
            name,
            arguments,
            body,
            return_type,
            documentation,
            deprecation,
            external_fsharp,
            ..
        } = f;
        let name = name.as_ref().map(|n| n.1.as_str()).unwrap_or("_");

        let body = match external_fsharp {
            Some((ref module_name, ref fn_name, _)) => {
                // TODO: look into tracking references to the external function in Call expressions
                // and call them directly instead? Or maybe add an inline specifier for this [instead]?

                let calling_args = if arguments.is_empty() {
                    "()".to_doc()
                } else {
                    join(arguments.iter().map(|arg| self.arg_name(arg)), " ".to_doc())
                };

                // If the "module" qualifier is a file path, assume that fn_name is fully qualified
                let qualifier = if module_name.contains("/") || module_name.contains("\\") {
                    _ = self.external_files.insert(module_name);
                    nil()
                } else {
                    docvec![module_name, "."]
                };

                docvec![qualifier, fn_name, " ", calling_args]
            }

            None => {
                docvec![
                    "begin",
                    line()
                        .append(self.statements(body, Some(return_type)))
                        .nest(INDENT)
                        .group(),
                    line(),
                    "end"
                ]
            }
        };

        let args = self.fun_args(arguments);
        let docs = match documentation {
            Some((_, ref doc)) => {
                let mut comment_lines = doc.split('\n').collect::<Vec<_>>();

                // Remove any trailing empty lines
                if comment_lines.last().map_or(false, |line| line.is_empty()) {
                    _ = comment_lines.pop();
                }

                join(
                    comment_lines
                        .iter()
                        .map(|doc_line| docvec!["///", doc_line]),
                    line(),
                )
                .append(line())
            }
            None => nil(),
        };

        let deprecation_doc = match deprecation {
            Deprecation::Deprecated { message } => {
                docvec!["[<System.Obsolete(", self.string(message), ")>]", line()]
            }
            Deprecation::NotDeprecated => nil(),
        };

        let return_type = self.type_to_fsharp(return_type);

        let a = args.clone().to_pretty_string(80);
        // TODO: Make this less magic
        let (entry_point_annotation, args) = if name == "main" {
            match &arguments[..] {
                [TypedArg {
                    names: ArgNames::Named { name, .. },
                    ..
                }] => (
                    "[<EntryPoint>]".to_doc().append(line()),
                    EcoString::from(format!("({}: string[])", name)).to_doc(),
                ),
                []
                | [TypedArg {
                    names: ArgNames::Discard { .. },
                    ..
                }] => (
                    "[<EntryPoint>]".to_doc().append(line()),
                    "(_: string[])".to_doc(),
                ),
                _ => (nil(), args),
            }
        } else {
            (nil(), args)
        };

        // For now, since we mark all modules as recursive, we don't need to mark
        // functions as recursive.
        docvec![
            docs,
            deprecation_doc,
            entry_point_annotation,
            "let ",
            self.map_publicity(f.publicity),
            name,
            " ",
            args,
            // ": ",
            // return_type,
            " = ",
            body
        ]
    }

    fn arg_name(&self, arg: &TypedArg) -> Document<'a> {
        arg.names
            .get_variable_name()
            .map(|n| n.to_doc())
            .unwrap_or_else(|| "_".to_doc())
    }

    /// Function definition arguments
    fn fun_args(&self, arguments: &[TypedArg]) -> Document<'a> {
        if arguments.is_empty() {
            "()".to_doc()
        } else {
            join(
                arguments.iter().map(|arg| {
                    docvec![self.arg_name(arg), ": ", self.type_to_fsharp(&arg.type_)]
                        .surround("(", ")")
                }),
                " ".to_doc(),
            )
            .group()
        }
    }

    /// Anonymous functions
    fn fun(&self, args: &'a [TypedArg], body: &'a [TypedStatement]) -> Document<'a> {
        docvec![
            "fun",
            self.fun_args(args),
            " -> begin ",
            self.statements(body, None).nest(INDENT),
            break_("", " "),
            "end"
        ]
        .group()
    }

    fn statement(&self, s: &'a TypedStatement) -> (Document<'a>, Option<Document<'a>>) {
        let mut last_var = None;
        let statement_doc = match s {
            Statement::Expression(expr) => {
                last_var = None;
                self.expression(expr)
            }
            TypedStatement::Assignment(Assignment {
                value,
                pattern:
                    Pattern::StringPrefix {
                        left_side_string: prefix,
                        right_side_assignment,
                        left_side_assignment: maybe_label,
                        ..
                    },
                ..
            }) => {
                // TODO: Add warning suppression when this is encountered:
                // #nowarn "25" // Incomplete pattern matches on this expression.
                let suffix_binding_name: Document<'a> = match right_side_assignment {
                    AssignName::Variable(right) => {
                        let v = right.to_doc();

                        last_var = Some(v.clone());
                        v
                    }
                    AssignName::Discard(_) => {
                        last_var = Some(self.expression(value).to_doc());
                        "_".to_doc()
                    }
                };

                docvec![
                    "let (",
                    prelude_functions::STRING_PATTERN_PARTS,
                    " ",
                    self.string(prefix),
                    " (",
                    match maybe_label {
                        Some((prefix_label, _)) => docvec![prefix_label, ", "],
                        None => "_, ".to_doc(),
                    },
                    suffix_binding_name,
                    ")) = ",
                    self.expression(value).to_doc(),
                ]
            }

            Statement::Assignment(a) => {
                let (name, value) = self.get_assignment_info(a);

                last_var = Some(name.clone());
                match a.value.as_ref() {
                    TypedExpr::Case { .. } => {
                        docvec![
                            "let ",
                            name,
                            " = ",
                            line().nest(INDENT),
                            value.group().nest(INDENT)
                        ]
                    }
                    _ => docvec!["let ", name, " = ", value],
                }
            }
            Statement::Use(_) => docvec!["// TODO: Implement use statements"],
        };

        (statement_doc, last_var)
    }

    fn statements(&self, s: &'a [TypedStatement], return_type: Option<&Type>) -> Document<'a> {
        let mut last_var = None;
        let mut res = s
            .iter()
            .map(|s| {
                let statement_info = self.statement(s);
                last_var = statement_info.1;
                statement_info.0
            })
            .collect::<Vec<Document<'a>>>();

        // Can't end on an assignment in F# unless it returns Unit

        if let Some(last_var) = last_var {
            match return_type {
                Some(return_type) => {
                    if !return_type.is_nil() {
                        res.push(last_var);
                    }
                }
                None => {
                    res.push(last_var);
                }
            }
        }

        join(res, line()).group()
    }

    fn santitize_name(&self, name: &'a EcoString) -> Document<'a> {
        join(
            name.split("/").map(|s| {
                if is_reserved_word(s) {
                    s.to_doc().surround("``", "``")
                } else {
                    s.to_doc()
                }
            }),
            ".".to_doc(),
        )
    }
    fn string_inner(&self, value: &str) -> Document<'a> {
        let content = unicode_escape_sequence_pattern()
            // `\\u`-s should not be affected, so that "\\u..." is not converted to
            // "\\u...". That's why capturing groups is used to exclude cases that
            // shouldn't be replaced.
            .replace_all(value, |caps: &Captures<'_>| {
                let slashes = caps.get(1).map_or("", |m| m.as_str());
                let unicode = caps.get(3).map_or("", |m| m.as_str());

                if slashes.len() % 2 == 0 {
                    // TODO: See if we can emit a warning here because it probably wasn't intentional
                    format!("{slashes}u{{{unicode}}}") // return the original string
                } else {
                    format!("{slashes}u{unicode}")
                }
            })
            .to_string();
        EcoString::from(content).to_doc()
    }

    fn string(&self, value: &str) -> Document<'a> {
        self.string_inner(value).surround("\"", "\"")
    }
    fn expression(&self, expr: &'a TypedExpr) -> Document<'a> {
        match expr {
            TypedExpr::Int { value, .. } => value.to_doc(),
            TypedExpr::Float { value, .. } => value.to_doc(),
            TypedExpr::String { value, .. } => self.string(value.as_str()),
            TypedExpr::Block { statements, .. } => self.block(statements),
            TypedExpr::Pipeline {
                assignments,
                finally,
                ..
            } => self.pipeline(assignments, finally),

            TypedExpr::Var { name, .. } => match name.as_str() {
                "Nil" => "()".to_doc(),
                "True" => "true".to_doc(),
                "False" => "false".to_doc(),
                _ => name.to_doc(),
            },
            TypedExpr::Fn { args, body, .. } => self.fun(args, body),
            TypedExpr::List { elements, .. } => {
                join(elements.iter().map(|e| self.expression(e)), "; ".to_doc()).surround("[", "]")
            }

            TypedExpr::Call { fun, args, .. } => match fun.as_ref() {
                TypedExpr::Var {
                    constructor:
                        ValueConstructor {
                            variant:
                                ValueConstructorVariant::Record {
                                    constructors_count: 1,
                                    arity,
                                    field_map: Some(ref field_map),
                                    ..
                                },
                            ..
                        },
                    ..
                } if *arity == field_map.fields.len() as u16 => {
                    // Every constructor field must have a label to be a record type
                    // println!("record instantiation: {:#?}", expr);
                    self.record_instantiation(field_map, args)
                }
                _ => {
                    println!("function call: {:#?}", expr);
                    self.function_call(fun, args)
                }
            },

            TypedExpr::BinOp {
                left, right, name, ..
            } => self.binop(name, left, right),

            TypedExpr::Case {
                subjects, clauses, ..
            } => self.case(subjects, clauses),

            TypedExpr::Tuple { elems, .. } => self.tuple(elems.iter().map(|e| self.expression(e))),

            // Special case for double negation
            TypedExpr::NegateInt { value, .. } => {
                // Special case for double negation
                if let TypedExpr::NegateInt { .. } = value.as_ref() {
                    "-".to_doc()
                        .append(self.expression(value).surround("(", ")"))
                } else {
                    "-".to_doc().append(self.expression(value))
                }
            }

            TypedExpr::Todo { message, .. } => self.todo(message),
            TypedExpr::Panic { message, .. } => self.panic_(message),
            TypedExpr::RecordAccess { label, record, .. } => self.record_access(record, label),
            TypedExpr::RecordUpdate { args, spread, .. } => {
                // If the target of the update is the result of a pipeline, it needs to be
                // surrounded in parentheses
                let old_var_name = match spread.deref() {
                    TypedExpr::Pipeline { .. } => self.expression(spread).surround("(", ")"),
                    _ => self.expression(spread),
                };

                let new_values = args.iter().map(|arg| {
                    let child_expr = match &arg.value {
                        // If the child here is a pipe operation, we need to indent at least
                        // one more space so that it starts on a column after the `with` keyword
                        TypedExpr::Pipeline { .. } => self.expression(&arg.value).nest(1),
                        _ => self.expression(&arg.value),
                    };

                    docvec![arg.label.clone(), " = ", child_expr].group()
                });

                docvec![
                    "{ ",
                    old_var_name,
                    " with ",
                    join(new_values, "; ".to_doc()).force_break(),
                    " }"
                ]
            }
            TypedExpr::ModuleSelect { .. } => "// TODO: TypedExpr::ModuleSelect".to_doc(),
            TypedExpr::TupleIndex { tuple, index, .. } => self.tuple_index(tuple, index),
            TypedExpr::BitArray { .. } => "// TODO: TypedExpr::BitArray".to_doc(),
            TypedExpr::NegateBool { .. } => "// TODO: TypedExpr::NegateBool".to_doc(),
            TypedExpr::Invalid { .. } => "// TODO: TypedExpr::Invalid".to_doc(),
        }
    }

    fn tuple_index(&self, tuple: &'a TypedExpr, index: &'a u64) -> Document<'a> {
        // TODO: Add warning suppression when this is encountered:
        // #nowarn "3220" // This method or property is not normally used from F# code, use an explicit tuple pattern for deconstruction instead.
        docvec![self.expression(tuple), ".Item", index + 1]
    }

    fn record_instantiation(
        &self,
        field_map: &'a FieldMap,
        args: &'a [CallArg<TypedExpr>],
    ) -> Document<'a> {
        let field_map = invert_field_map(field_map);

        let args = args.iter().enumerate().map(|(i, arg)| {
            let label = field_map.get(&(i as u32)).expect("Index out of bounds");
            docvec![
                self.santitize_name(label).to_doc(),
                " = ",
                self.expression(&arg.value)
            ]
        });

        join(args, "; ".to_doc()).group().surround("{ ", " }")
    }

    fn function_call(&self, fun: &'a TypedExpr, args: &'a [CallArg<TypedExpr>]) -> Document<'a> {
        let args = if args.is_empty() {
            "()".to_doc()
        } else {
            " ".to_doc().append(
                join(
                    args.iter()
                        .map(|a| self.expression(&a.value).surround("(", ")")),
                    " ".to_doc(),
                )
                .group(),
            )
        };
        let fun_expr = self.expression(fun);
        // If for some reason we're doing an IIFE, we need to wrap it in parens
        let fun_expr = match fun {
            TypedExpr::Fn { .. } => fun_expr.surround("(", ")"),
            _ => fun_expr,
        };
        fun_expr.append(args).group()
    }

    fn record_access(&self, record: &'a TypedExpr, label: &'a EcoString) -> Document<'a> {
        let record_expr = self.expression(record);
        let label_expr = label.to_doc();
        docvec![record_expr, ".", label_expr]
    }

    fn todo(&self, message: &'a Option<Box<TypedExpr>>) -> Document<'a> {
        match message {
            Some(message) => "failwith "
                .to_doc()
                .append(self.expression(message.as_ref()).surround("(", ")")),
            None => "failwith \"Not implemented\"".to_doc(),
        }
    }

    fn panic_(&self, message: &'a Option<Box<TypedExpr>>) -> Document<'a> {
        match message {
            Some(message) => "failwith "
                .to_doc()
                .append(self.expression(message.as_ref()).surround("(", ")")),
            None => "failwith \"Panic encountered\"".to_doc(),
        }
    }

    fn tuple(&self, elements: impl IntoIterator<Item = Document<'a>>) -> Document<'a> {
        join(elements, ", ".to_doc()).surround("(", ")")
    }
    fn case(&self, subjects: &'a [TypedExpr], clauses: &'a [TypedClause]) -> Document<'a> {
        let subjects_doc = if subjects.len() == 1 {
            self.expression(
                subjects
                    .first()
                    .expect("f# case printing of single subject"),
            )
        } else {
            self.tuple(subjects.iter().map(|s| self.expression(s)))
        };

        let clauses = join(
            clauses
                .iter()
                .map(|c| "| ".to_doc().append(self.clause(c).group())),
            line(),
        )
        .group();
        docvec![
            docvec!["match ", subjects_doc, " with"].group(),
            line(),
            clauses
        ]
        .group()
    }

    fn clause(&self, clause: &'a TypedClause) -> Document<'a> {
        let Clause {
            guard,
            pattern: pat,
            alternative_patterns,
            then,
            ..
        } = clause;
        let mut then_doc = None;

        let additional_guards = vec![];
        let patterns_doc = join(
            std::iter::once(pat)
                .chain(alternative_patterns)
                .map(|patterns| {
                    let patterns_doc = if patterns.len() == 1 {
                        let p = patterns.first().expect("Single pattern clause printing");
                        self.pattern(p)
                    } else {
                        self.tuple(patterns.iter().map(|p| self.pattern(p)))
                    };

                    patterns_doc
                }),
            " | ".to_doc(),
        );
        let guard = self.optional_clause_guard(guard.as_ref(), additional_guards);
        if then_doc.is_none() {
            then_doc = Some(self.clause_consequence(then));
        }
        patterns_doc.append(
            guard.append(" ->").append(
                line()
                    .append(then_doc.clone().to_doc())
                    .nest(INDENT)
                    .group(),
            ),
        )
    }

    fn clause_consequence(&self, consequence: &'a TypedExpr) -> Document<'a> {
        match consequence {
            TypedExpr::Block { statements, .. } => self.statement_sequence(statements),
            _ => self.expression(consequence),
        }
    }

    fn statement_sequence(&self, statements: &'a [TypedStatement]) -> Document<'a> {
        let documents = statements.iter().map(|e| self.statement(e).0.group());

        let documents = join(documents, line());
        if statements.len() == 1 {
            documents
        } else {
            documents.force_break()
        }
    }
    fn optional_clause_guard(
        &self,
        guard: Option<&'a TypedClauseGuard>,
        additional_guards: Vec<Document<'a>>,
    ) -> Document<'a> {
        let guard_doc = guard.map(|guard| self.bare_clause_guard(guard));

        let guards_count = guard_doc.iter().len() + additional_guards.len();
        let guards_docs = additional_guards.into_iter().chain(guard_doc).map(|guard| {
            if guards_count > 1 {
                guard.surround("(", ")")
            } else {
                guard
            }
        });
        let doc = join(guards_docs, " && ".to_doc());
        if doc.is_empty() {
            doc
        } else {
            " when ".to_doc().append(doc)
        }
    }
    fn clause_guard(&self, guard: &'a TypedClauseGuard) -> Document<'a> {
        match guard {
            // Binary operators are wrapped in parens
            ClauseGuard::Or { .. }
            | ClauseGuard::And { .. }
            | ClauseGuard::Equals { .. }
            | ClauseGuard::NotEquals { .. }
            | ClauseGuard::GtInt { .. }
            | ClauseGuard::GtEqInt { .. }
            | ClauseGuard::LtInt { .. }
            | ClauseGuard::LtEqInt { .. }
            | ClauseGuard::GtFloat { .. }
            | ClauseGuard::GtEqFloat { .. }
            | ClauseGuard::LtFloat { .. }
            | ClauseGuard::LtEqFloat { .. }
            | ClauseGuard::AddInt { .. }
            | ClauseGuard::AddFloat { .. }
            | ClauseGuard::SubInt { .. }
            | ClauseGuard::SubFloat { .. }
            | ClauseGuard::MultInt { .. }
            | ClauseGuard::MultFloat { .. }
            | ClauseGuard::DivInt { .. }
            | ClauseGuard::DivFloat { .. }
            | ClauseGuard::RemainderInt { .. } => "("
                .to_doc()
                .append(self.bare_clause_guard(guard))
                .append(")"),

            // Other expressions are not
            ClauseGuard::Constant(_)
            | ClauseGuard::Not { .. }
            | ClauseGuard::Var { .. }
            | ClauseGuard::TupleIndex { .. }
            | ClauseGuard::FieldAccess { .. }
            | ClauseGuard::ModuleSelect { .. } => self.bare_clause_guard(guard),
        }
    }
    fn bare_clause_guard(&self, guard: &'a TypedClauseGuard) -> Document<'a> {
        match guard {
            ClauseGuard::Not { expression, .. } => {
                docvec!["not ", self.bare_clause_guard(expression)]
            }

            ClauseGuard::Or { left, right, .. } => self
                .clause_guard(left)
                .append(" || ")
                .append(self.clause_guard(right)),

            ClauseGuard::And { left, right, .. } => self
                .clause_guard(left)
                .append(" && ")
                .append(self.clause_guard(right)),

            ClauseGuard::Equals { left, right, .. } => self
                .clause_guard(left)
                .append(" = ")
                .append(self.clause_guard(right)),

            ClauseGuard::NotEquals { left, right, .. } => self
                .clause_guard(left)
                .append(" <> ")
                .append(self.clause_guard(right)),

            ClauseGuard::GtInt { left, right, .. } | ClauseGuard::GtFloat { left, right, .. } => {
                self.clause_guard(left)
                    .append(" > ")
                    .append(self.clause_guard(right))
            }

            ClauseGuard::GtEqInt { left, right, .. }
            | ClauseGuard::GtEqFloat { left, right, .. } => self
                .clause_guard(left)
                .append(" >= ")
                .append(self.clause_guard(right)),

            ClauseGuard::LtInt { left, right, .. } | ClauseGuard::LtFloat { left, right, .. } => {
                self.clause_guard(left)
                    .append(" < ")
                    .append(self.clause_guard(right))
            }

            ClauseGuard::LtEqInt { left, right, .. }
            | ClauseGuard::LtEqFloat { left, right, .. } => self
                .clause_guard(left)
                .append(" =< ")
                .append(self.clause_guard(right)),

            ClauseGuard::AddInt { left, right, .. } | ClauseGuard::AddFloat { left, right, .. } => {
                self.clause_guard(left)
                    .append(" + ")
                    .append(self.clause_guard(right))
            }

            ClauseGuard::SubInt { left, right, .. } | ClauseGuard::SubFloat { left, right, .. } => {
                self.clause_guard(left)
                    .append(" - ")
                    .append(self.clause_guard(right))
            }

            ClauseGuard::MultInt { left, right, .. }
            | ClauseGuard::MultFloat { left, right, .. } => self
                .clause_guard(left)
                .append(" * ")
                .append(self.clause_guard(right)),

            ClauseGuard::DivInt { left, right, .. } | ClauseGuard::DivFloat { left, right, .. } => {
                self.clause_guard(left)
                    .append(" / ")
                    .append(self.clause_guard(right))
            }

            ClauseGuard::RemainderInt { left, right, .. } => self
                .clause_guard(left)
                .append(" % ")
                .append(self.clause_guard(right)),

            // TODO: Only local variables are supported and the typer ensures that all
            // ClauseGuard::Vars are local variables
            ClauseGuard::Var { name, .. } => name.to_doc(),

            //ClauseGuard::TupleIndex { tuple, index, .. } => tuple_index(tuple, index),

            // ClauseGuard::FieldAccess {
            //     container, index, ..
            // } => tuple_index_inline(container, index.expect("Unable to find index") + 1),

            // ClauseGuard::ModuleSelect { literal, .. } => const_inline(literal),
            ClauseGuard::Constant(c) => self.constant_expression(c),
            _ => docvec!["// TODO: Implement other guard types"],
        }
    }

    fn binop(&self, name: &'a BinOp, left: &'a TypedExpr, right: &'a TypedExpr) -> Document<'a> {
        let operand = match name {
            // Boolean logic
            BinOp::And => "&&",
            BinOp::Or => "||",

            // Equality
            BinOp::Eq => "=",
            BinOp::NotEq => "<>",

            // Order comparison
            BinOp::LtInt | BinOp::LtFloat => "<",
            BinOp::LtEqInt | BinOp::LtEqFloat => "<=",
            BinOp::GtInt | BinOp::GtFloat => ">",
            BinOp::GtEqInt | BinOp::GtEqFloat => ">=",

            // Maths
            BinOp::AddInt | BinOp::AddFloat => "+",
            BinOp::SubInt | BinOp::SubFloat => "-",
            BinOp::MultInt | BinOp::MultFloat => "*",
            BinOp::DivInt | BinOp::DivFloat => "/",
            BinOp::RemainderInt => "%",

            // Strings
            BinOp::Concatenate => "+",
        };
        self.expression(left)
            .append(" ")
            .append(operand)
            .append(" ")
            .append(self.expression(right))
    }

    /// Implement pipeline (|>) expressions
    fn pipeline(&self, assignments: &'a [TypedAssignment], finally: &'a TypedExpr) -> Document<'a> {
        let mut documents = Vec::with_capacity((assignments.len() + 1) * 3);

        for a in assignments {
            let (name, value) = self.get_assignment_info(a);

            documents.push(docvec!["let ", name, " = ", value]);
            documents.push(line());
        }

        documents.push(self.expression(finally).surround("(", ")"));

        self.wrap_in_begin_end(documents.to_doc())
    }

    fn wrap_in_begin_end(&self, expr: Document<'a>) -> Document<'a> {
        "begin"
            .to_doc()
            .append(line())
            .nest(INDENT)
            .append(expr.nest(INDENT).group())
            .append(line().append("end"))
    }

    fn block(&self, s: &'a [TypedStatement]) -> Document<'a> {
        "begin"
            .to_doc()
            .append(line())
            .nest(INDENT)
            .append(self.statements(s, None).nest(INDENT).group())
            .append(line().append("end"))
    }

    fn get_assignment_info(&self, assignment: &'a TypedAssignment) -> (Document<'a>, Document<'a>) {
        let name = self.pattern(&assignment.pattern);
        let value = self.expression(&assignment.value);
        (name, value)
    }

    fn pattern(&self, p: &'a Pattern<Arc<Type>>) -> Document<'a> {
        match p {
            Pattern::Int { value, .. } => value.to_doc(),
            Pattern::Float { value, .. } => value.to_doc(),
            Pattern::String { value, .. } => self.string(value.as_str()),
            Pattern::Variable { name, .. } => name.to_doc(),
            Pattern::Discard { name, .. } => name.to_doc(),
            Pattern::List { elements, tail, .. } => {
                let head = join(elements.iter().map(|e| self.pattern(e)), "; ".to_doc())
                    .surround("[", "]");

                match tail {
                    Some(tail) => head.append("::").append(self.pattern(tail)),
                    None => head,
                }
            }
            Pattern::Tuple { elems, .. } => {
                join(elems.iter().map(|e| self.pattern(e)), ", ".to_doc()).surround("(", ")")
            }

            Pattern::StringPrefix {
                left_side_string: prefix,
                right_side_assignment,
                left_side_assignment: maybe_prefix_label,
                ..
            } => {
                // TODO: Add warning suppression when this is encountered:
                // #nowarn "25" // Incomplete pattern matches on this expression.
                let suffix_binding_name: Document<'a> = match right_side_assignment {
                    AssignName::Variable(right) => right.to_doc(),
                    AssignName::Discard(_) => "_".to_doc(),
                };

                match maybe_prefix_label {
                    None => {
                        docvec![
                            prelude_functions::STRING_PATTERN_PREFIX,
                            " ",
                            self.string(prefix),
                            " ",
                            suffix_binding_name,
                        ]
                    }
                    Some((prefix_label, _)) => {
                        docvec![
                            prelude_functions::STRING_PATTERN_PARTS,
                            " ",
                            self.string(prefix),
                            " (",
                            prefix_label.to_doc(),
                            ", ",
                            suffix_binding_name,
                            ")"
                        ]
                    }
                }
            }
            Pattern::BitArray { segments, .. } => {
                let segments_docs = segments.iter().map(|s| self.pattern(&s.value));
                join(segments_docs, "; ".to_doc()).surround("[|", "|]")
            }
            Pattern::VarUsage {
                name, constructor, ..
            } => {
                let v = &constructor
                    .as_ref()
                    .expect("Constructor not found for variable usage")
                    .variant;
                match v {
                    ValueConstructorVariant::ModuleConstant { literal, .. } => {
                        self.constant_expression(literal)
                    }
                    _ => name.to_doc(),
                }
            }
            Pattern::Invalid { .. } => panic!("invalid patterns should not reach code generation"),
            Pattern::Assign {
                name, pattern: p, ..
            } => self.pattern(p).append(" as ").append(name),

            Pattern::Constructor {
                constructor:
                    Inferred::Known(PatternConstructor {
                        field_map: Some(ref field_map),
                        ..
                    }),
                spread,
                arguments,
                ..
            } if arguments.len() == field_map.fields.len() => {
                let field_map = invert_field_map(field_map);

                let args = arguments.iter().enumerate().filter_map(|(i, arg)| {
                    if spread.is_some() && arg.value.is_discard() {
                        return None;
                    }

                    let label = match &arg.label {
                        Some(label) => Some(label.to_doc()),
                        None => field_map.get(&(i as u32)).map(|label| label.to_doc()),
                    };

                    label.map(|label| docvec![label.to_doc(), " = ", self.pattern(&arg.value)])
                });
                join(args, "; ".to_doc()).group().surround("{ ", " }")
            }

            Pattern::Constructor { name, type_, .. } if type_.is_bool() && name == "True" => {
                "true".to_doc()
            }
            Pattern::Constructor { name, type_, .. } if type_.is_bool() && name == "False" => {
                "false".to_doc()
            }

            Pattern::Constructor { type_, .. } if type_.is_nil() => "()".to_doc(),

            Pattern::Constructor {
                name,
                arguments,
                constructor,
                ..
            } => {
                let args = arguments.iter().map(|arg| self.pattern(&arg.value));
                let args = if arguments.is_empty() {
                    "()".to_doc()
                } else {
                    join(args, ", ".to_doc()).surround("(", ")")
                };
                docvec![name.to_doc(), args].surround("(", ")")
            }
        }
    }

    fn type_to_fsharp(&self, t: &Type) -> Document<'a> {
        if t.is_nil() {
            return "unit".to_doc();
        }

        match t {
            Type::Named { name, args, .. } => {
                let name = match name.as_str() {
                    "Int" | "int" => "int".to_doc(),
                    "Float" | "float" => "float".to_doc(),
                    "String" | "string" => "string".to_doc(),
                    "Bool" | "bool" => "bool".to_doc(),
                    "Nil" => "unit".to_doc(),
                    "List" => "list".to_doc(),
                    _ => name.to_doc(),
                };
                if args.is_empty() {
                    name.to_doc()
                } else {
                    name.to_doc()
                        .append("<")
                        .append(join(
                            args.iter().map(|arg| self.type_to_fsharp(arg)),
                            ", ".to_doc(),
                        ))
                        .append(">")
                }
            }
            Type::Fn { args, retrn, .. } => {
                let arg_types = args
                    .iter()
                    .map(|arg| self.type_to_fsharp(arg))
                    .collect::<Vec<Document<'a>>>();

                let arg_types = if arg_types.is_empty() {
                    "unit".to_doc()
                } else {
                    join(arg_types, " -> ".to_doc())
                };

                let return_type = self.type_to_fsharp(retrn);
                docvec![arg_types, " -> ", return_type]
            }
            Type::Tuple { elems } => {
                join(elems.iter().map(|t| self.type_to_fsharp(t)), "; ".to_doc()).surround("(", ")")
            }
            Type::Var { type_ } => {
                let borrowed = type_.borrow();
                match borrowed.deref() {
                    TypeVar::Link { type_ } => self.type_to_fsharp(type_),
                    TypeVar::Unbound { id } => EcoString::from(format!("'u{}", id)).to_doc(),
                    TypeVar::Generic { id } => EcoString::from(format!("'t{}", id)).to_doc(),
                }
            }
        }
    }

    fn module_constant(&self, constant: &'a ModuleConstant<Arc<Type>, EcoString>) -> Document<'a> {
        let name = constant.name.as_str();

        match constant.value.deref() {
            Constant::Int { .. } | Constant::Float { .. } | Constant::String { .. } => {
                docvec![
                    "[<Literal>]",
                    line(),
                    "let ",
                    self.map_publicity(constant.publicity),
                    name,
                    " = ",
                    self.constant_expression(&constant.value)
                ]
            }
            _ => docvec![
                "let ",
                self.map_publicity(constant.publicity),
                name,
                " = ",
                self.constant_expression(&constant.value)
            ],
        }
    }

    fn constant_expression(&self, expression: &'a TypedConstant) -> Document<'a> {
        match expression {
            Constant::Int { value, .. } => value.to_doc(),
            Constant::Float { value, .. } => value.to_doc(),
            Constant::String { value, .. } => self.string(value),
            Constant::Tuple { elements, .. } => {
                self.tuple(elements.iter().map(|e| self.constant_expression(e)))
            }

            Constant::List { elements, .. } => {
                self.list(elements.iter().map(|e| self.constant_expression(e)))
            }

            Constant::Record { type_, name, .. } if type_.is_bool() && name == "True" => {
                "true".to_doc()
            }
            Constant::Record { type_, name, .. } if type_.is_bool() && name == "False" => {
                "false".to_doc()
            }
            Constant::Record { type_, .. } if type_.is_nil() => "()".to_doc(),

            Constant::Record {
                args,
                module,
                name: type_name,
                tag,
                type_,
                field_map,
                ..
            } => {
                if let Some(constructor) = self.module.type_info.values.get(type_name) {
                    if let ValueConstructorVariant::Record {
                        name,
                        constructors_count,
                        field_map: Some(field_map),
                        arity,
                        ..
                    } = &constructor.variant
                    {
                        // Is a genuine record constructor if:
                        // only one constructor exists
                        // constructor name is the same as the type name
                        // All fields have labels

                        if *constructors_count == 1u16
                            && name == type_name
                            && *arity == field_map.fields.len() as u16
                        {
                            let field_map = invert_field_map(field_map);

                            println!("arity: {}", arity);
                            println!("field_map: {:#?}", field_map);
                            let args = args.iter().enumerate().map(|(i, arg)| {
                                let label =
                                    field_map.get(&(i as u32)).expect("Index out of bounds");
                                docvec![label.to_doc(), " = ", self.constant_expression(&arg.value)]
                            });

                            return join(args, "; ".to_doc()).group().surround("{ ", " }");
                        }
                    }
                }

                // If there's no arguments and the type is a function that takes
                // arguments then this is the constructor being referenced, not the
                // function being called.
                if let Some(arity) = type_.fn_arity() {
                    if args.is_empty() && arity != 0 {
                        let arity = arity as u16;
                        return self.type_constructor(type_.clone(), None, type_name, arity);
                    }
                }

                if field_map.is_none() && args.is_empty() {
                    return tag.to_doc();
                }

                // if let Type::Custom type_.deref()

                let field_values: Vec<_> = args
                    .iter()
                    .map(|arg| self.constant_expression(&arg.value))
                    .collect();

                self.construct_type(
                    module.as_ref().map(|(module, _)| module.as_str()),
                    type_name,
                    field_values,
                )
            }

            Constant::BitArray { .. } => "//TODO: Constant::BitArray".to_doc(),

            Constant::Var { name, module, .. } => {
                match module {
                    None => self.santitize_name(name),
                    Some((module, _)) => {
                        // JS keywords can be accessed here, but we must escape anyway
                        // as we escape when exporting such names in the first place,
                        // and the imported name has to match the exported name.
                        docvec![module, ".", self.santitize_name(name)]
                    }
                }
            }

            Constant::StringConcatenation { left, right, .. } => {
                let left = self.constant_expression(left);
                let right = self.constant_expression(right);
                docvec!(left, " + ", right)
            }

            Constant::Invalid { .. } => {
                panic!("invalid constants should not reach code generation")
            }
        }
    }

    fn type_constructor(
        &self,
        type_: Arc<Type>,
        qualifier: Option<&'a str>,
        name: &'a str,
        arity: u16,
    ) -> Document<'a> {
        if type_.is_bool() && name == "True" {
            "true".to_doc()
        } else if type_.is_bool() {
            "false".to_doc()
        } else if type_.is_nil() {
            "undefined".to_doc()
        } else if arity == 0 {
            match qualifier {
                Some(module) => docvec![module, ".", name, "()"],
                None => docvec![name, "()"],
            }
        } else {
            let vars = (0..arity).map(|i| EcoString::from(format!("var{i}")).to_doc());
            let body = self.construct_type(qualifier, name, vars.clone());

            docvec!(
                "fun ",
                self.wrap_args(vars),
                " -> begin",
                break_("", " "),
                body
            )
            .nest(INDENT)
            .append(break_("", " "))
            .group()
            .append("end")
        }
    }

    fn wrap_args(&self, args: impl IntoIterator<Item = Document<'a>>) -> Document<'a> {
        break_("", "")
            .append(join(args, break_(",", ", ")))
            .nest(INDENT)
            .append(break_("", ""))
            .surround("(", ")")
            .group()
    }

    fn construct_type(
        &self,
        module: Option<&'a str>,
        name: &'a str,
        arguments: impl IntoIterator<Item = Document<'a>>,
    ) -> Document<'a> {
        let mut any_arguments = false;
        let arguments = join(
            arguments.into_iter().inspect(|_| {
                any_arguments = true;
            }),
            break_(",", ", "),
        );
        let arguments = docvec![break_("", ""), arguments].nest(INDENT);
        let name = if let Some(module) = module {
            docvec![module, ".", name]
        } else {
            name.to_doc()
        };
        if any_arguments {
            docvec![name, "(", arguments, break_(",", ""), ")"].group()
        } else {
            docvec![name, "()"]
        }
    }

    fn list(&self, elements: impl IntoIterator<Item = Document<'a>>) -> Document<'a> {
        join(elements, "; ".to_doc()).group().surround("[", "]")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Unsupported { feature: String, location: SrcSpan },
}
fn invert_field_map(field_map: &FieldMap) -> HashMap<&u32, &EcoString> {
    field_map
        .fields
        .iter()
        .map(|(k, v)| (v, k))
        .collect::<HashMap<_, _>>()
}