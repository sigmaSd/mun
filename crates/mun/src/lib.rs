use std::cell::RefCell;
use std::env;
use std::rc::Rc;
use std::time::Duration;

use anyhow::anyhow;
use clap::{App, AppSettings, Arg, ArgMatches, SubCommand};
use mun_compiler::{Config, DisplayColor, Target};
use mun_project::MANIFEST_FILENAME;
use mun_runtime::{invoke_fn, ReturnTypeReflection, Runtime, RuntimeBuilder};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Copy, Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    Success,
    Error,
}

impl Into<ExitStatus> for bool {
    fn into(self) -> ExitStatus {
        if self {
            ExitStatus::Success
        } else {
            ExitStatus::Error
        }
    }
}

pub fn run_with_args<T, I>(args: I) -> Result<ExitStatus, anyhow::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = App::new("mun")
        .version(env!("CARGO_PKG_VERSION"))
        .author("The Mun Project Developers")
        .about("The Mun executable enables compiling and running standalone Mun code")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .subcommand(
            SubCommand::with_name("build")
                .arg(
                    Arg::with_name("manifest-path")
                        .long("manifest-path")
                        .takes_value(true)
                        .help(&format!("Path to {}", MANIFEST_FILENAME))
                )
                .arg(
                    Arg::with_name("watch")
                        .long("watch")
                        .help("Run the compiler in watch mode.\
                        Watch input files and trigger recompilation on changes.",)
                )
                .arg(
                    Arg::with_name("opt-level")
                        .short("O")
                        .long("opt-level")
                        .takes_value(true)
                        .help("optimize with possible levels 0-3"),
                )
                .arg(
                    Arg::with_name("target")
                        .long("target")
                        .takes_value(true)
                        .help("target triple for which code is compiled"),
                )
                .arg(
                    Arg::with_name("color")
                        .long("color")
                        .takes_value(true)
                        .possible_values(&["enable", "auto", "disable"])
                        .help("color text in terminal"),
                )
                .about("Compiles a local Mun file into a module"),
        )
        .subcommand(
            SubCommand::with_name("start")
                .arg(
                    Arg::with_name("LIBRARY")
                        .help("Sets the library to use")
                        .required(true)
                        .index(1),
                )
                .arg(
                    Arg::with_name("entry")
                        .long("entry")
                        .takes_value(true)
                        .help("the function entry point to call on startup"),
                )
                .arg(
                    Arg::with_name("delay")
                        .long("delay")
                        .takes_value(true)
                        .help("how much to delay received filesystem events (in ms). This allows bundling of identical events, e.g. when several writes to the same file are detected. A high delay will make hot reloading less responsive. (defaults to 10 ms)"),
                ),
        )
        .subcommand(
            SubCommand::with_name("language-server")
        )
        .subcommand("new")
            .about("Create a new mun package at <path>")
            .arg(opt("quiet", "No output printed to stdout").short("q"))
            .arg(Arg::with_name("path").required(true))
        .get_matches_from_safe(args);

    match matches {
        Ok(matches) => match matches.subcommand() {
            ("build", Some(matches)) => build(matches),
            ("new", Some(matches)) => new(matches),
            ("language-server", Some(matches)) => language_server(matches),
            ("start", Some(matches)) => start(matches).map(|_| ExitStatus::Success),
            _ => unreachable!(),
        },
        Err(e) => {
            eprint!("{}", e.message);
            Ok(ExitStatus::Error)
        }
    }
}

/// Find a Mun manifest file in the specified directory or one of its parents.
fn find_manifest(directory: &Path) -> Option<PathBuf> {
    let mut current_dir = Some(directory);
    while let Some(dir) = current_dir {
        let manifest_path = dir.join(MANIFEST_FILENAME);
        if manifest_path.exists() {
            return Some(manifest_path);
        }
        current_dir = dir.parent();
    }
    None
}

fn new(matches: &ArgMatches) -> Result<ExitStatus, anyhow::Error> {
    const default_file_content: &[u8] = b"\
    fn entry() -> usize {
        1+1
    }
    ";
    log::trace!("starting new");
    // unwrap is safe because "path" is required by clap
    let path = match matches.value_of("path").unwrap();
    let path = std::path::Path::new(path);
    std::fs::create_dir_all(path)?;

    let entry_file = std::fs::File::create(path.join("main.mun"));
    entry_file.write_all(default_file_content);
}

/// This method is invoked when the executable is run with the `build` argument indicating that a
/// user requested us to build a project in the current directory or one of its parent directories.
///
/// The `bool` return type for this function indicates whether the process should exit with a
/// success or failure error code.
fn build(matches: &ArgMatches) -> Result<ExitStatus, anyhow::Error> {
    log::trace!("starting build");

    let options = compiler_options(matches)?;

    // Locate the manifest
    let manifest_path = match matches.value_of("manifest-path") {
        None => {
            let current_dir =
                std::env::current_dir().expect("could not determine currrent working directory");
            find_manifest(&current_dir).ok_or_else(|| {
                anyhow::anyhow!(
                    "could not find {} in '{}' or a parent directory",
                    MANIFEST_FILENAME,
                    current_dir.display()
                )
            })?
        }
        Some(path) => std::fs::canonicalize(Path::new(path))
            .map_err(|_| anyhow::anyhow!("'{}' does not refer to a valid manifest path", path))?,
    };

    log::info!("located build manifest at: {}", manifest_path.display());

    if matches.is_present("watch") {
        mun_compiler_daemon::compile_and_watch_manifest(&manifest_path, options)
    } else {
        mun_compiler::compile_manifest(&manifest_path, options)
    }
    .map(Into::into)
}

/// Starts the runtime with the specified library and invokes function `entry`.
fn start(matches: &ArgMatches) -> Result<ExitStatus, anyhow::Error> {
    let runtime = runtime(matches)?;

    let borrowed = runtime.borrow();
    let entry_point = matches.value_of("entry").unwrap_or("main");
    let fn_definition = borrowed
        .get_function_definition(entry_point)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Failed to obtain entry point '{}'", entry_point),
            )
        })?;

    if let Some(ret_type) = fn_definition.prototype.signature.return_type() {
        let type_guid = &ret_type.guid;
        if *type_guid == bool::type_guid() {
            let result: bool = invoke_fn!(runtime, entry_point).map_err(|e| anyhow!("{}", e))?;

            println!("{}", result)
        } else if *type_guid == f64::type_guid() {
            let result: f64 = invoke_fn!(runtime, entry_point).map_err(|e| anyhow!("{}", e))?;

            println!("{}", result)
        } else if *type_guid == i64::type_guid() {
            let result: i64 = invoke_fn!(runtime, entry_point).map_err(|e| anyhow!("{}", e))?;

            println!("{}", result)
        } else {
            return Err(anyhow!(
                "Only native Mun return types are supported for entry points. Found: {}",
                ret_type.name()
            ));
        };
        Ok(ExitStatus::Success)
    } else {
        #[allow(clippy::unit_arg)]
        invoke_fn!(runtime, entry_point)
            .map(|_: ()| ExitStatus::Success)
            .map_err(|e| anyhow!("{}", e))
    }
}

fn compiler_options(matches: &ArgMatches) -> Result<mun_compiler::Config, anyhow::Error> {
    let optimization_lvl = match matches.value_of("opt-level") {
        Some("0") => mun_compiler::OptimizationLevel::None,
        Some("1") => mun_compiler::OptimizationLevel::Less,
        None | Some("2") => mun_compiler::OptimizationLevel::Default,
        Some("3") => mun_compiler::OptimizationLevel::Aggressive,
        _ => return Err(anyhow!("Only optimization levels 0-3 are supported")),
    };

    let display_color = matches
        .value_of("color")
        .map(ToOwned::to_owned)
        .or_else(|| env::var("MUN_TERMINAL_COLOR").ok())
        .map(|value| match value.as_str() {
            "disable" => DisplayColor::Disable,
            "enable" => DisplayColor::Enable,
            _ => DisplayColor::Auto,
        })
        .unwrap_or(DisplayColor::Auto);

    Ok(Config {
        target: matches
            .value_of("target")
            .map_or_else(Target::host_target, Target::search)?,
        optimization_lvl,
        out_dir: None,
        display_color,
    })
}

fn runtime(matches: &ArgMatches) -> Result<Rc<RefCell<Runtime>>, anyhow::Error> {
    let builder = RuntimeBuilder::new(
        matches.value_of("LIBRARY").unwrap(), // Safe because its a required arg
    );

    let builder = if let Some(delay) = matches.value_of("delay") {
        let delay: u64 = delay.parse()?;
        builder.set_delay(Duration::from_millis(delay))
    } else {
        builder
    };

    builder.spawn()
}

/// This function is invoked when the executable is invoked with the `language-server` argument. A
/// Mun language server is started ready to serve language information about one or more projects.
///
/// The `bool` return type for this function indicates whether the process should exit with a
/// success or failure error code.
fn language_server(_matches: &ArgMatches) -> Result<ExitStatus, anyhow::Error> {
    mun_language_server::run_server().map_err(|e| anyhow::anyhow!("{}", e))?;
    Ok(ExitStatus::Success)
}

#[cfg(test)]
mod test {
    use crate::find_manifest;
    use mun_project::MANIFEST_FILENAME;
    use tempdir::TempDir;

    #[test]
    fn test_find_manifest() {
        let dir = TempDir::new("test_find_manifest").unwrap();
        let path = dir.path();
        let manifest_path = path.join(MANIFEST_FILENAME);

        assert_eq!(find_manifest(path), None);

        std::fs::write(&manifest_path, "").unwrap();
        assert_eq!(find_manifest(path).as_ref(), Some(&manifest_path));

        let subdir_path = path.join("some/random/subdir");
        std::fs::create_dir_all(&subdir_path).unwrap();
        assert_eq!(find_manifest(&subdir_path).as_ref(), Some(&manifest_path));
    }
}
