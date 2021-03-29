use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Deserialize, Debug)]
struct Target {
  name: String,
  crate_types: Vec<String>,
  edition: String,
  src_path: String,
}

#[derive(Deserialize, Debug)]
struct Dependency {
  index: usize,
}

#[derive(Deserialize, Debug)]
struct Unit {
  pkg_id: String,
  target: Target,
  dependencies: Vec<Dependency>,
  features: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct UnitGraph {
  units: Vec<Unit>,
  roots: Vec<usize>,
}

impl UnitGraph {
  fn run_cargo_and_build() -> Result<Self> {
    let cargo_output = Command::new("cargo")
      .args(&["check", "--unit-graph", "-Z", "unstable-options"])
      .output()?
      .stdout;
    Ok(serde_json::from_slice::<UnitGraph>(&cargo_output)?)
  }

  fn find_unit_containing(&self, source_path: &Path) -> Option<&Unit> {
    self.units.iter().find(|unit| {
      let src_path = Path::new(&unit.target.src_path);
      match src_path.parent() {
        Some(src_dir) => source_path.ancestors().any(|ancestor| ancestor == src_dir),
        None => false,
      }
    })
  }
}

fn gather_rmeta_paths() -> Result<HashMap<String, String>> {
  let re = Regex::new(r"lib(.+)-[\w\d]+.rmeta")?;
  Ok(
    fs::read_dir("target/debug/deps")?
      .map(|file| {
        Ok({
          let path = file?.path();
          let path_str = path.to_str().context("Couldn't convert path")?;
          if let Some(file_name) = path.file_name() {
            let file_name = file_name.to_str().context("Couldn't convert file name")?;
            re.captures(file_name).map(|capture| {
              (
                capture.get(1).unwrap().as_str().to_owned(),
                path_str.to_owned(),
              )
            })
          } else {
            None
          }
        })
      })
      .collect::<Result<Vec<_>>>()?
      .into_iter()
      .filter_map(|x| x)
      .collect::<HashMap<_, _>>(),
  )
}

pub fn generate_rustc_flags(source_path: impl AsRef<Path>) -> Result<Vec<String>> {
  let source_path = source_path.as_ref();

  let sysroot = String::from_utf8(
    Command::new("rustc")
      .args(&["--print", "sysroot"])
      .output()?
      .stdout,
  )?;
  let sysroot = sysroot.trim().to_string();

  let graph = UnitGraph::run_cargo_and_build()?;

  let target_unit = graph.find_unit_containing(&source_path).context(format!(
    "Could not find unit with source directory for {}",
    source_path.display()
  ))?;

  // Run cargo check to generate dependency rmetas
  {
    for dependency in &target_unit.dependencies {
      let dep_unit = &graph.units[dependency.index];
      let mut pkg_parts = dep_unit.pkg_id.split(" ");
      let pkg_name = pkg_parts.next().context("Missing name from pkg_id")?;
      let pkg_version = pkg_parts.next().context("Missing version from pkg_id")?;
      let check = Command::new("cargo")
        .args(&[
          "check",
          "--package",
          &format!("{}:{}", pkg_name, pkg_version),
        ])
        .output()?;
      if !check.status.success() {
        bail!(
          "cargo check failed with error: {}",
          String::from_utf8(check.stderr)?
        );
      }
    }
  }

  let rmeta_paths = gather_rmeta_paths()?;

  #[rustfmt::skip]
  let unit_flags = vec![
    "rustc".into(),    
    
    "--crate-name".into(), target_unit.target.name.clone(),

    // TODO: what if there are multiple crate types?
    "--crate-type".into(), target_unit.target.crate_types[0].clone(),

    "--sysroot".into(), sysroot,

    // Path must be the crate root file, NOT the sliced file
    target_unit.target.src_path.clone(),

    format!("--edition={}", target_unit.target.edition),

    "-L".into(), "dependency=target/debug/deps".into(),

    // Avoids ICE looking for MIR data?
    "--emit=dep-info,metadata".into(),
  ];

  let feature_flags = target_unit
    .features
    .iter()
    .map(|feature| vec!["--cfg".into(), format!("feature=\"{}\"", feature)])
    .flatten();

  let extern_flags = target_unit
    .dependencies
    .iter()
    .map(|dep| {
      let dep_unit = &graph.units[dep.index];

      // packages like `percent-encoding` are translated to `percent_encoding`
      let package_name = dep_unit.target.name.replace("-", "_");

      let rmeta_path = &rmeta_paths
        .get(&package_name)
        .expect(&format!("Missing rmeta for `{}`", package_name));

      vec![
        "--extern".into(),
        format!("{}={}", package_name, rmeta_path),
      ]
    })
    .flatten();
  Ok(
    unit_flags
      .into_iter()
      .chain(feature_flags)
      .chain(extern_flags)
      .collect(),
  )
}
