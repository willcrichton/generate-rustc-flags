use anyhow::{bail, Context as AnyhowContext, Result};
use cargo::{
  core::{
    compiler::{
      build_map, compile, extern_args, lto, BuildPlan, CompileMode, Context, CrateType,
      DefaultExecutor, Executor, JobQueue, Unit, UnitInterner,
    },
    Workspace,
  },
  ops::{create_bcx, CompileFilter, CompileOptions, FilterRule, LibRule, Packages},
  util::config::Config,
};
use std::env;
use std::process::Command;
use std::sync::Arc;
use std::{
  collections::HashMap,
  path::{Path},
};

pub use cargo::core::resolver::CliFeatures;

fn collect_units(cx: &Context, unit: &Unit) -> Vec<Unit> {
  cx.unit_deps(unit)
    .iter()
    .map(|dep| collect_units(cx, &dep.unit).into_iter())
    .flatten()
    .chain(vec![unit.clone()].into_iter())
    .collect()
}

pub fn generate_rustc_flags(
  source_path: impl AsRef<Path>,
  features: CliFeatures,
  lib_only: bool,
) -> Result<Vec<String>> {
  let source_path = source_path.as_ref();

  let rustc = env::var_os("RUSTC")
    .map(|s| s.into_string().unwrap())
    .unwrap_or("rustc".to_string());
  let sysroot = String::from_utf8(
    Command::new(rustc)
      .args(&["--print", "sysroot"])
      .output()?
      .stdout,
  )?;
  let sysroot = sysroot.trim().to_string();

  let config = Config::default()?;
  let manifest_path = Path::new("./Cargo.toml").canonicalize()?;
  let workspace = Workspace::new(manifest_path.as_ref(), &config)?;
  let mut compile_opts = CompileOptions::new(&config, CompileMode::Check { test: false })?;
  compile_opts.spec = Packages::Default;
  compile_opts.cli_features = features;

  if lib_only {
    compile_opts.filter = CompileFilter::Only {
      all_targets: false,
      lib: LibRule::Default,
      bins: FilterRule::Just(vec![]),
      examples: FilterRule::Just(vec![]),
      tests: FilterRule::Just(vec![]),
      benches: FilterRule::Just(vec![]),
    };
  }

  let interner = UnitInterner::new();
  let bcx = create_bcx(&workspace, &compile_opts, &interner)?;
  let mut cx = Context::new(&bcx)?;

  cx.lto = lto::generate(&bcx)?;
  cx.prepare_units()?;
  cx.prepare()?;
  build_map(&mut cx)?;

  let all_units = bcx
    .roots
    .iter()
    .map(|root| collect_units(&cx, root).into_iter())
    .flatten()
    .collect::<Vec<_>>();

  let target_unit = {
    let matches = all_units
      .iter()
      .filter(|root| {
        let unit_src_path = root.target.src_path().path().unwrap();
        match unit_src_path.parent() {
          Some(src_dir) => source_path.ancestors().any(|ancestor| ancestor == src_dir),
          None => false,
        }
      })
      .collect::<Vec<_>>();

    match matches.len() {
      0 => bail!("Could not find unit for path {}", source_path.display()),
      1 => matches[0],
      _ => matches
        .into_iter()
        .find(|unit| {
          unit
            .target
            .rustc_crate_types()
            .iter()
            .any(|ty| *ty == CrateType::Lib)
        })
        .context("No lib target w/ multiple targets")?,
    }
  };

  // TODO: generate these from build_base_args
  #[rustfmt::skip]
  let unit_flags = vec![
    "rustc".into(),

    "--crate-name".into(), target_unit.target.crate_name(),

    // TODO: what if there are multiple crate types?
    "--crate-type".into(), target_unit.target.kind().rustc_crate_types()[0].as_str().to_string(),

    "--sysroot".into(), sysroot,

    // Path must be the crate root file, NOT the sliced file
    format!("{}", target_unit.target.src_path().path().unwrap().display()),

    format!("--edition={}", target_unit.target.edition()),

    "-L".into(), format!("{}", cx.files().layout(target_unit.kind).deps().display()),

    // Avoids ICE looking for MIR data?
    "--emit=dep-info,metadata".into(),
  ];

  let feature_flags = target_unit
    .features
    .iter()
    .map(|feature| vec!["--cfg".into(), format!("feature=\"{}\"", feature)])
    .flatten();

  let extern_flags = extern_args(&cx, target_unit, &mut false)?
    .into_iter()
    .map(|s| s.into_string().unwrap());

  let pkg = &target_unit.pkg;
  let mut env = vec![
    ("CARGO_PKG_VERSION", pkg.version().to_string()),
    ("CARGO_PKG_NAME", pkg.name().to_string()),
    (
      "CARGO_MANIFEST_DIR",
      format!("{}", manifest_path.parent().unwrap().display()),
    ),
    ("CARGO_PKG_VERSION_MAJOR", pkg.version().major.to_string()),
    ("CARGO_PKG_VERSION_MINOR", pkg.version().minor.to_string()),
    ("CARGO_PKG_VERSION_PATCH", pkg.version().patch.to_string()),
  ]
  .into_iter()
  .map(|(k, v)| (k.to_string(), v))
  .collect::<HashMap<_, _>>();

  if let Some(target_meta) = cx.find_build_script_metadata(target_unit) {
    let build_unit = cx.find_build_script_unit(target_unit).unwrap();
    let mut queue = JobQueue::new(&bcx);
    let mut plan = BuildPlan::new();
    let exec = Arc::new(DefaultExecutor) as Arc<dyn Executor>;
    compile(&mut cx, &mut queue, &mut plan, &build_unit, &exec, false)?;
    queue.execute(&mut cx, &mut plan)?;

    env.insert(
      "OUT_DIR".into(),
      format!("{}", cx.files().build_script_out_dir(&build_unit).display()),
    );

    let outputs = cx.build_script_outputs.lock().unwrap();
    let output = outputs.get(target_meta).unwrap();
    env.extend(output.env.clone().into_iter());
  }

  for (k, v) in env {
    env::set_var(k, v);
  }

  Ok(
    unit_flags
      .into_iter()
      .chain(feature_flags)
      .chain(extern_flags)
      .collect(),
  )
}
