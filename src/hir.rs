use rustc_data_structures::sync::{Lrc, Send};
use rustc_data_structures::unord::UnordSet;
use rustc_driver::abort_on_err;
use rustc_errors::emitter::{Emitter, EmitterWriter};
use rustc_errors::json::JsonEmitter;
use rustc_errors::ErrorGuaranteed;
use rustc_feature::UnstableFeatures;
use rustc_hir::def_id::LocalDefId;
use rustc_interface::interface;
use rustc_middle::ty::TyCtxt;
use rustc_session::config::{
    parse_crate_types_from_list, parse_externs, rustc_optgroups, CodegenOptions, ErrorOutputType,
    Input, Options, UnstableOptions,
};
use rustc_session::early_error_no_abort;
use rustc_session::search_paths::SearchPath;
use rustc_session::{config, early_error, getopts};
use rustc_span::source_map::{FilePathMapping, SourceMap};
use rustc_span::FileName;

use std::io::{self, Read};
use std::marker;
use std::path::PathBuf;
use std::sync::LazyLock;

/// If you need more information than what is provided by
/// [`with_ast_parser`](crate::with_ast_parser), this is the function you'll use.
///
/// **VERY IMPORTANT TO NOTE**: if you want to run this code on a crate with dependencies, you'll
/// need to pass the according options so that `rustc` knows where to look for them. otherwise it
/// will simply fail to compile and the `callback` won't be called. A good example of the list
/// of the expected arguments can be seen when you run `cargo build -v`.
///
/// Don't forget to take a look at the [`TyCtxt`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_middle/ty/struct.TyCtxt.html)
/// and at the [`Map`](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_middle/hir/map/struct.Map.html)
/// documentations.
///
/// And to make things much simpler, I strongly recommend to use
/// the [HIR visitor](https://doc.rust-lang.org/nightly/nightly-rustc/rustc_hir/intravisit/trait.Visitor.html).
/// (You can take a look at how to use it with `examples/hir.rs`.)
pub fn with_tyctxt<T: marker::Send, F: FnOnce(TyCtxt<'_>) -> T + marker::Send>(
    rustc_args: &[String],
    callback: F,
) -> Result<T, String> {
    // Most of this code comes from rustdoc.
    rustc_driver::init_rustc_env_logger();
    let args = rustc_driver::args::arg_expand_all(rustc_args);

    let mut options = getopts::Options::new();
    for option in rustc_optgroups() {
        (option.apply)(&mut options);
    }
    let matches = match options.parse(&args[..]) {
        Ok(m) => m,
        Err(err) => {
            early_error(ErrorOutputType::default(), &err.to_string());
        }
    };

    // Note that we discard any distinction between different non-zero exit
    // codes from `from_matches` here.
    let config = match create_config(&matches) {
        Some(opts) => opts,
        None => return Err("Failed to create_config".to_owned()),
    };

    interface::run_compiler(config, |compiler| {
        let sess = compiler.session();

        if sess.opts.describe_lints {
            early_error(
                ErrorOutputType::default(),
                "`describe-lints` option is not allowed",
            );
        }

        compiler.enter(|queries| {
            {
                // FIXME: very likely unneeded.
                let _expansion = abort_on_err(queries.expansion(), sess);
            }

            if sess.diagnostic().has_errors_or_lint_errors().is_some() {
                sess.fatal("Compilation failed, aborting");
            }

            let global_ctxt = abort_on_err(queries.global_ctxt(), sess);

            global_ctxt.enter(|tcx| Ok(callback(tcx)))
        })
    })
}

fn make_input(
    error_format: ErrorOutputType,
    free_matches: &[String],
    diag: &rustc_errors::Handler,
) -> Result<Option<Input>, ErrorGuaranteed> {
    if free_matches.len() == 1 {
        let ifile = &free_matches[0];
        if ifile == "-" {
            let mut src = String::new();
            if io::stdin().read_to_string(&mut src).is_err() {
                // Immediately stop compilation if there was an issue reading
                // the input (for example if the input stream is not UTF-8).
                let reported = early_error_no_abort(
                    error_format,
                    "couldn't read from stdin, as it did not contain valid UTF-8",
                );
                return Err(reported);
            }
            Ok(Some(Input::Str {
                name: FileName::anon_source_code(&src),
                input: src,
            }))
        } else {
            Ok(Some(Input::File(PathBuf::from(ifile))))
        }
    } else if free_matches.is_empty() {
        diag.struct_err("missing file operand").emit();
        Ok(None)
    } else {
        diag.struct_err("too many file operands").emit();
        Ok(None)
    }
}

fn new_handler(
    error_format: ErrorOutputType,
    source_map: Option<Lrc<SourceMap>>,
    diagnostic_width: Option<usize>,
    unstable_opts: &UnstableOptions,
) -> rustc_errors::Handler {
    let fallback_bundle =
        rustc_errors::fallback_fluent_bundle(rustc_errors::DEFAULT_LOCALE_RESOURCES, false);
    let emitter: Box<dyn Emitter + Send> = match error_format {
        ErrorOutputType::HumanReadable(kind) => {
            let (short, color_config) = kind.unzip();
            Box::new(
                EmitterWriter::stderr(
                    color_config,
                    source_map.map(|sm| sm as _),
                    None,
                    fallback_bundle,
                    short,
                    unstable_opts.teach,
                    diagnostic_width,
                    false,
                    unstable_opts.track_diagnostics,
                )
                .ui_testing(unstable_opts.ui_testing),
            )
        }
        ErrorOutputType::Json {
            pretty,
            json_rendered,
        } => {
            let source_map =
                source_map.unwrap_or_else(|| Lrc::new(SourceMap::new(FilePathMapping::empty())));
            Box::new(
                JsonEmitter::stderr(
                    None,
                    source_map,
                    None,
                    fallback_bundle,
                    pretty,
                    json_rendered,
                    diagnostic_width,
                    false,
                    unstable_opts.track_diagnostics,
                )
                .ui_testing(unstable_opts.ui_testing),
            )
        }
    };

    rustc_errors::Handler::with_emitter_and_flags(
        emitter,
        unstable_opts.diagnostic_handler_flags(true),
    )
}

fn create_config(matches: &getopts::Matches) -> Option<interface::Config> {
    let color = config::parse_color(matches);
    let config::JsonConfig { json_rendered, .. } = config::parse_json(matches);
    let error_format = config::parse_error_format(matches, color, json_rendered);
    let diagnostic_width = matches.opt_get("diagnostic-width").unwrap_or_default();

    let codegen_options = CodegenOptions::build(matches, error_format);
    let unstable_opts = UnstableOptions::build(matches, error_format);

    let diag = new_handler(error_format, None, diagnostic_width, &unstable_opts);

    let (lint_opts, describe_lints, lint_cap) = config::get_cmd_lint_options(matches, error_format);

    let input = match make_input(error_format, &matches.free, &diag) {
        Ok(Some(i)) => i,
        Ok(None) => {
            return None;
        }
        Err(e) => {
            diag.struct_err(&format!("Failed to parse input: {:?}", e))
                .emit();
            return None;
        }
    };

    let libs = matches
        .opt_strs("L")
        .iter()
        .map(|s| SearchPath::from_cli_opt(s, error_format))
        .collect();
    let externs = parse_externs(matches, &unstable_opts, error_format);

    let cfgs = matches.opt_strs("cfg");
    let check_cfgs = matches.opt_strs("check-cfg");

    let crate_types = match parse_crate_types_from_list(matches.opt_strs("crate-type")) {
        Ok(types) => types,
        Err(e) => {
            diag.struct_err(&format!("unknown crate type: {}", e))
                .emit();
            return None;
        }
    };
    let crate_name = matches.opt_str("crate-name");

    let sessopts = config::Options {
        maybe_sysroot: matches.opt_str("sysroot").map(PathBuf::from),
        search_paths: libs,
        crate_types,
        lint_opts,
        lint_cap,
        cg: codegen_options,
        externs,
        target_triple: config::parse_target_triple(matches, error_format),
        unstable_features: UnstableFeatures::from_environment(crate_name.as_deref()),
        actually_rustdoc: false,
        unstable_opts,
        error_format,
        diagnostic_width,
        edition: config::parse_crate_edition(matches),
        describe_lints,
        crate_name,
        test: false,
        ..Options::default()
    };

    Some(interface::Config {
        opts: sessopts,
        crate_cfg: interface::parse_cfgspecs(cfgs),
        crate_check_cfg: interface::parse_check_cfg(check_cfgs),
        input,
        output_file: None,
        output_dir: None,
        file_loader: None,
        lint_caps: Default::default(),
        parse_sess_created: None,
        register_lints: None,
        override_queries: Some(|_sess, providers, _external_providers| {
            // Most lints will require typechecking, so just don't run them.
            providers.lint_mod = |_, _| {};
            // Prevent `rustc_hir_analysis::check_crate` from calling `typeck` on all bodies.
            providers.typeck_item_bodies = |_, _| {};
            // hack so that `used_trait_imports` won't try to call typeck
            providers.used_trait_imports = |_, _| {
                static EMPTY_SET: LazyLock<UnordSet<LocalDefId>> = LazyLock::new(UnordSet::default);
                &EMPTY_SET
            };
        }),
        make_codegen_backend: None,
        registry: rustc_driver::diagnostics_registry(),
    })
}
