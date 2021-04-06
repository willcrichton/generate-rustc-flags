use anyhow::{bail, Context as AnyhowContext, Result};
use cargo::{
  core::{
    compiler::{build_map, extern_args, lto, CompileMode, Context, CrateType, UnitInterner},
    Workspace,
  },
  ops::{create_bcx, CompileOptions, Packages},
  util::config::Config,
};
use std::process::Command;
use std::{collections::HashMap, path::Path};

pub use cargo::core::resolver::CliFeatures;

pub fn generate_rustc_flags(
  source_path: impl AsRef<Path>,
  features: CliFeatures,
) -> Result<(Vec<String>, HashMap<String, String>)> {
  let source_path = source_path.as_ref();

  let sysroot = String::from_utf8(
    Command::new("rustc")
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
  let interner = UnitInterner::new();
  let bcx = create_bcx(&workspace, &compile_opts, &interner)?;
  let mut cx = Context::new(&bcx)?;

  cx.lto = lto::generate(&bcx)?;
  cx.prepare_units()?;
  cx.prepare()?;
  build_map(&mut cx)?;

  let target_unit = {
    let matches = bcx
      .roots
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

  let mut env = HashMap::new();
  env.insert(
    "CARGO_PKG_VERSION".into(),
    target_unit.pkg.version().to_string(),
  );
  env.insert("CARGO_PKG_NAME".into(), target_unit.pkg.name().to_string());

  // TODO: NOT WORKING
  if let Some(target_meta) = cx.find_build_script_metadata(target_unit) {
    if let Some(output) = cx.build_script_outputs.lock().unwrap().get(target_meta) {
      // for cfg in output.cfgs.iter() {
      //   rustdoc.arg("--cfg").arg(cfg);
      // }
      for &(ref name, ref value) in output.env.iter() {
        env.insert(name.to_owned(), value.to_owned());
      }
    }
  }

  Ok((
    unit_flags
      .into_iter()
      .chain(feature_flags)
      .chain(extern_flags)
      .collect(),
    env,
  ))
}
