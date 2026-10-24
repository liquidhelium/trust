//! Generation of function bodies and `NativeFuncData` statics.
//!
//! `gen_function` compiles parameter destructuring + direct body
//! translation; `gen_func_data` writes the function's `NativeFuncData`
//! descriptor by hand (parameter table derived from the closure AST).

use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode as _};

use super::ctx::{Ctx, Env, bind_local};
use super::naming::{escape, sanitize, sanitize_key};

impl Ctx {
    /// Generate one function: parameter destructuring + direct body
    /// translation.
    pub(super) fn gen_function(
        &mut self,
        fun: &str,
        slot: &str,
        closure: ast::Closure,
        env: &mut Env,
    ) -> String {
        let mut body = String::new();
        let mut fn_env = env.clone();
        // Whether `__st` is needed (function body / defaults reference
        // module slots) is tracked during generation.
        self.used_state = false;
        // Function-body context: return compiles to a Rust return, legal
        // only in this context.
        self.in_fn += 1;
        // A top-level function is likewise a closure in typst
        // (CapturesVisitor captures free names): referencing a module
        // binding inside the body is a capture, **read-only** (mirrors
        // `Binding::capture`).
        self.in_closure += 1;
        // Mirrors eval_closure's binding protocol (parameters bound one by
        // one in declaration order, sink special):
        // - sink_size = total positional arguments − explicit positional
        //   parameters (saturating);
        // - at the Spread position, `sink_size` positional arguments are
        //   consumed from the **queue head** and stashed;
        // - explicit positional parameters (including those after the
        //   sink) are expected in order from the remainder;
        // - the trailing sink = take() (everything remaining, including
        //   named) plus the stashed queue-head items extended back in.
        // Binding the explicit parameters first and taking the whole
        // remainder afterwards was once wrong: with `(..args, body)` and
        // multi-config arguments, body wrongly took the first config and
        // the content landed in args.pos(), so a merge-dicts call received
        // content and crashed on .keys() (located by breaking on the wrong
        // construction).
        let num_pos_params = closure
            .params()
            .children()
            .filter(|p| matches!(p, ast::Param::Pos(_)))
            .count();
        let mut sink_stash: Vec<(Option<String>, String)> = Vec::new();
        // The total positional-argument count must be taken **before any
        // binding** (sink_size's denominator is the full count; computing
        // it at the Spread position would already have earlier parameters
        // deducted).
        let has_sink = closure
            .params()
            .children()
            .any(|p| matches!(p, ast::Param::Spread(_)));
        if has_sink {
            let _ = writeln!(body, "    let __n_pos_args = __args.to_pos().len();");
        }
        for param in closure.params().children() {
            match param {
                ast::Param::Pos(ast::Pattern::Normal(ast::Expr::Ident(ident))) => {
                    let name = ident.as_str().to_string();
                    let rust = bind_local(&mut fn_env, &name);
                    let _ = writeln!(
                        body,
                        "    let mut {rust} = rt::param_pos(__args, \"{}\", span)?;",
                        escape(&name)
                    );
                }
                ast::Param::Pos(ast::Pattern::Placeholder(_)) => {
                    // A placeholder consumes one positional argument
                    // (mirrors eval_closure's
                    // `args.expect::<Value>("pattern parameter")`).
                    let _ = writeln!(
                        body,
                        "    let _ = rt::param_pos(__args, \"pattern parameter\", span)?;"
                    );
                }
                ast::Param::Pos(pattern) => {
                    // Destructuring parameter (mirrors eval_closure:
                    // "pattern parameter").
                    let value = "rt::param_pos(__args, \"pattern parameter\", span)?".to_string();
                    body.push_str(&self.destructure(pattern, value, &mut fn_env, false));
                }
                ast::Param::Named(named) => {
                    let name = named.name().as_str().to_string();
                    // Defaults are evaluated and materialized into slots at
                    // definition time (module_init) and only read at call
                    // time: evaluation timing matches typst (once, at
                    // creation).
                    let def_slot = format!("DEF_{slot}_{}", sanitize_key(&name));
                    self.used_state = true;
                    let rust = bind_local(&mut fn_env, &name);
                    let _ = writeln!(
                        body,
                        "    let mut {rust} = match __args.named::<Value>(\"{}\")? {{\n\
                         \x20       Some(value) => value,\n\
                         \x20       None => rt::read(&__st.{def_slot})?,\n\
                         \x20   }};",
                        escape(&name)
                    );
                }
                ast::Param::Spread(spread) => {
                    let stash = format!("__sink_pos_{}", sink_stash.len());
                    let _ = writeln!(
                        body,
                        "    let {stash} = __args.consume(__n_pos_args.saturating_sub({num_pos_params}))?;"
                    );
                    let sink = spread.sink_ident().map(|ident| {
                        let name = ident.as_str().to_string();
                        bind_local(&mut fn_env, &name)
                    });
                    sink_stash.push((sink, stash));
                }
            }
        }
        // sink = take() (the remainder, including named) in declaration
        // order + the stashed queue-head positional items (mirrors
        // eval_closure's trailing `remaining_args.items.extend(sink_pos)`).
        for (sink, stash) in sink_stash {
            let target = sink.as_deref().unwrap_or("_sink");
            let _ = writeln!(
                body,
                "    let mut {target} = Value::Args(__args.take());\n    if let Value::Args(__sink_inner) = &mut {target} {{ __sink_inner.items.extend({stash}.into_iter()); }}"
            );
        }
        let _ = writeln!(body, "    rt::finish(__args, span)?;");

        // Bare-expression function body (let-expression hoisting shell;
        // block bodies re-scan themselves in block()).
        let hoist = self.open_let_hoists(std::iter::once(closure.body().to_untyped()));
        let value = self.expr(closure.body(), &mut fn_env);
        self.close_let_hoists();
        let value = if hoist.is_empty() {
            value
        } else {
            format!("{{ {} {value} }}", hoist)
        };
        let _ = writeln!(body, "    Ok({value})");

        // Prologue: state is unpacked from the first positional argument
        // (the stub always forwards it; mirrors the native-closure scheme
        // C: data travels with the function value, the global registry
        // lookup is gone). It must precede parameter unpacking — state
        // occupies the first positional argument.
        let prologue = if self.used_state {
            "    let __st = rt::take_state::<__State>(__args)?;\n"
        } else {
            ""
        };
        self.in_fn -= 1;
        self.in_closure = self.in_closure.saturating_sub(1);
        format!(
            "\n#[allow(non_snake_case, dead_code, unused_variables)]\nfn {fun}(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {{\n    let span = __args.span;\n{prologue}{body}}}\n"
        )
    }
pub(super) fn gen_closure_fn(
    &mut self,
    fun: &str,
    data: &str,
    closure: ast::Closure,
    capture_names: &[String],
    self_name: Option<&str>,
    params_src: &str,
) -> String {
    // The closure body's evaluation environment = its own parameters +
    // captures (all free names are in the capture tuple; nothing is
    // inherited from the outer env). has_st (body references __st: module
    // slot reads or in-body string imports) is decided after the body is
    // generated — prologue and tuple type are assembled afterwards.
    let mut fn_env = super::ctx::Env {
        locals: Vec::new(),
        module: Vec::new(),
        wildcards: Vec::new(),
    };
    let saved = self.used_state;
    self.used_state = false;
    self.in_fn += 1;
    self.in_closure += 1;
    // Captured-name frame: the capture Rust names materialized into the
    // environment for this closure level (mirroring typst's read-only
    // `Binding::capture` marker in the closure's captured scope) —
    // assignments / mutating methods landing on these names report a
    // read-only error at runtime.
    self.captured_locals.push(Default::default());

    // Bind the captured names first (the body's free names resolve to
    // these locals).
    for name in capture_names {
        let rust = bind_local(&mut fn_env, name);
        self.captured_locals
            .last_mut()
            .expect("captured-name frame is pushed on entry")
            .insert(rust);
    }
    // The self-reference name is injected into env **before** body
    // generation (it used to be bound only in the prologue-construction
    // section; at body-generation time the name was invisible, so the
    // recursion point fell to the global fallback — unknown variable).
    let mut self_binding: Option<(String, String)> = None;
    if let Some(name) = self_name {
        let rust = bind_local(&mut fn_env, name);
        self_binding = Some((name.to_string(), rust));
    }
    // Parameter binding: positional/destructuring/sink same as top level;
    // named parameters are already filled by the stub, taken as required.
    // Bound in declaration order; the sink consumes from the queue head
    // into a stash (same protocol as the top-level function prologue,
    // mirrors eval_closure; see the protocol comment at the top-level
    // function).
    let mut params_code = String::new();
    let num_pos_params = closure
        .params()
        .children()
        .filter(|p| matches!(p, ast::Param::Pos(_)))
        .count();
    let mut sink_stash: Vec<(Option<String>, String)> = Vec::new();
    let has_sink = closure
        .params()
        .children()
        .any(|p| matches!(p, ast::Param::Spread(_)));
    if has_sink {
        let _ = writeln!(params_code, "    let __n_pos_args = __args.to_pos().len();");
    }
    for param in closure.params().children() {
        match param {
            ast::Param::Pos(ast::Pattern::Normal(ast::Expr::Ident(ident))) => {
                let name = ident.as_str().to_string();
                let rust = bind_local(&mut fn_env, &name);
                let _ = writeln!(
                    params_code,
                    "    let mut {rust} = rt::param_pos(__args, \"{}\", span)?;",
                    escape(&name)
                );
            }
            ast::Param::Pos(ast::Pattern::Placeholder(_)) => {
                // A placeholder consumes one positional argument (same as
                // the top-level function prologue).
                let _ = writeln!(
                    params_code,
                    "    let _ = rt::param_pos(__args, \"pattern parameter\", span)?;"
                );
            }
            ast::Param::Pos(pattern) => {
                let value = "rt::param_pos(__args, \"pattern parameter\", span)?".to_string();
                params_code.push_str(&self.destructure(pattern, value, &mut fn_env, false));
            }
            ast::Param::Named(named) => {
                let name = named.name().as_str().to_string();
                let rust = bind_local(&mut fn_env, &name);
                let _ = writeln!(
                    params_code,
                    "    let mut {rust} = rt::param_named(__args, \"{}\", span)?;",
                    escape(&name)
                );
            }
            ast::Param::Spread(spread) => {
                let stash = format!("__sink_pos_{}", sink_stash.len());
                let _ = writeln!(
                    params_code,
                    "    let {stash} = __args.consume(__n_pos_args.saturating_sub({num_pos_params}))?;"
                );
                let sink = spread.sink_ident().map(|ident| {
                    let name = ident.as_str().to_string();
                    bind_local(&mut fn_env, &name)
                });
                sink_stash.push((sink, stash));
            }
        }
    }
    for (sink, stash) in sink_stash {
        let target = sink.as_deref().unwrap_or("_sink");
        let _ = writeln!(
            params_code,
            "    let mut {target} = Value::Args(__args.take());\n    if let Value::Args(__sink_inner) = &mut {target} {{ __sink_inner.items.extend({stash}.into_iter()); }}"
        );
    }
    let _ = writeln!(params_code, "    rt::finish(__args, span)?;");

    // Bare-expression body hoisting shell (same as gen_function; block
    // bodies re-scan themselves in block()).
    let hoist = self.open_let_hoists(std::iter::once(closure.body().to_untyped()));
    let value = self.expr(closure.body(), &mut fn_env);
    self.close_let_hoists();
    let value = if hoist.is_empty() {
        value
    } else {
        format!("{{ {} {value} }}", hoist)
    };
    let body_code = format!("    Ok({value})\n");

    // has_st decision (flags set during body/parameter code generation);
    // restore the outer counter and leave this closure's decision for the
    // caller to read.
    // has_st = the body's own decision; used_state is left for the caller
    // to read, then the outer counter is restored
    // (do not restore here — the outer read would be polluted and the
    // creation tuple and prologue shape would disagree).
    let has_st = self.used_state;
    self.in_fn -= 1;
    self.captured_locals.pop();
    self.in_closure = self.in_closure.saturating_sub(1);

    // State tuple and unpacking prologue (has_st decided afterwards). A
    // self-recursive closure: the tuple gets a trailing SelfFuncCell (a
    // once-per-instance rebuild cache for the stub itself); the prologue
    // keeps the slot.
    let mut tuple_t: Vec<&str> = Vec::new();
    let mut destruct: Vec<String> = Vec::new();
    if has_st {
        tuple_t.push("std::sync::Arc<__State>");
        destruct.push("__st".to_string());
    }
    for i in 0..capture_names.len() {
        tuple_t.push("Option<Value>");
        destruct.push(format!("__c{i}"));
    }
    let self_destruct;
    if self_name.is_some() {
        tuple_t.push("std::sync::Arc<rt::SelfFuncCell>");
        self_destruct = "__selfcell".to_string();
        destruct.push(self_destruct.clone());
    }
    let tuple_t = if tuple_t.is_empty() {
        "()".to_string()
    } else {
        tuple_t.join(", ")
    };
    let mut prologue = if self_name.is_some() {
        format!(
            "    let (({}), __slot) = rt::take_caps_self::<({tuple_t})>(__args)?;\n",
            destruct.join(", ")
        )
    } else if destruct.is_empty() {
        "    rt::take_caps::<()>(__args)?;\n".to_string()
    } else {
        format!(
            "    let ({}) = rt::take_caps::<({tuple_t})>(__args)?;\n",
            destruct.join(", ")
        )
    };
    // Captures unpack by name (None means the name was invisible at
    // creation time; call time reports unknown variable, matching native
    // capture semantics). Local names taken in env binding order.
    for (i, name) in capture_names.iter().enumerate() {
        let rust = fn_env
            .locals
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, r)| r.clone())
            .unwrap_or_else(|| format!("__cap_{i}"));
        let _ = writeln!(
            prologue,
            "    let mut {rust} = rt::cap(__c{i}, \"{}\", span)?;",
            escape(name)
        );
    }
    if let Some((name, rust)) = &self_binding {
        // Self-reference binding emission (injection timing into env is
        // noted above; the prologue goes here). The rebuild uses the
        // per-call-site static node (the same one as the creation point;
        // zero lookups, zero locks).
        let stub_src = trust_runtime::compose_stub(name, params_src);
        let node_name = self.closure_node_static(&stub_src);
        let _ = writeln!(
            prologue,
            "    let mut {rust} = Value::Func(rt::self_func_node(&__selfcell, &{data}, &{node_name}, &__slot));"
        );
    }

    format!(
        "\n#[allow(non_snake_case, dead_code, unused_variables)]\nfn {fun}(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {{\n    let span = __args.span;\n{prologue}{params_code}{body_code}}}\n"
    )
}
}

/// `NativeParamInfo` literal (metadata derived from the closure parameter
/// shapes).
fn format_param(name: &str, named: bool, positional: bool, variadic: bool) -> String {
    format!(
        "NativeParamInfo {{ name: \"{}\", docs: \"\", def_site: None, input: CastInfo::Any, default: None, positional: {positional}, named: {named}, variadic: {variadic}, required: {}, settable: false }}",
        escape(name),
        positional && !variadic
    )
}

pub(super) fn gen_func_data(data: &str, fun: &str, name: &str, closure: ast::Closure) -> String {
    let mut params = Vec::new();
    for param in closure.params().children() {
        match param {
            ast::Param::Pos(ast::Pattern::Normal(ast::Expr::Ident(ident))) => {
                params.push(format_param(ident.as_str(), false, true, false));
            }
            ast::Param::Pos(_) => params.push(format_param("_", false, true, false)),
            ast::Param::Named(named) => {
                // Named parameters bind by name only (args.named); metadata
                // stays consistent.
                params.push(format_param(named.name().as_str(), true, false, false))
            }
            ast::Param::Spread(_) => params.push(format_param("args", false, true, true)),
        }
    }
    let list = if params.is_empty() {
        "Vec::new()".into()
    } else {
        format!("vec![\n        {},\n    ]", params.join(",\n        "))
    };
    format!(
        "\n#[allow(non_snake_case, dead_code)]\nfn {fun}_params() -> Vec<NativeParamInfo> {{\n    {list}\n}}\n\
         #[allow(non_snake_case, dead_code)]\nstatic {data}: NativeFuncData = NativeFuncData {{\n    \
         function: NativeFuncPtr(&{fun}),\n    \
         name: \"{name}\",\n    \
         title: \"{name}\",\n    \
         docs: \"\",\n    \
         def_site: None,\n    \
         keywords: &[],\n    \
         contextual: false,\n    \
         scope: LazyLock::new(&rt::empty_scope),\n    \
         params: LazyLock::new(&{fun}_params),\n    \
         returns: LazyLock::new(&rt::any_cast_info),\n}};\n"
    )
}
