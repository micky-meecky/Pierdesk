use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/windows/pierdesk.rc");
    println!("cargo:rerun-if-changed=assets/windows/pierdesk.manifest");
    println!("cargo:rerun-if-changed=assets/brand/pierdesk.ico");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_AUTHORS");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_DESCRIPTION");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }

    let manifest_directory = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must provide CARGO_MANIFEST_DIR"),
    );
    let output_directory =
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must provide OUT_DIR to build scripts"));
    let resource_source = manifest_directory.join("assets/windows/pierdesk.rc");
    let compiled_resource = output_directory.join("pierdesk.res");
    let version_header = output_directory.join("pierdesk-version.h");
    let resource_compiler = env::var_os("RC").unwrap_or_else(|| OsString::from("rc.exe"));
    let package_version = env::var("CARGO_PKG_VERSION")
        .expect("Cargo must provide CARGO_PKG_VERSION to build scripts");
    let package_authors = env::var("CARGO_PKG_AUTHORS")
        .expect("Cargo must provide CARGO_PKG_AUTHORS to build scripts");
    let package_description = env::var("CARGO_PKG_DESCRIPTION")
        .expect("Cargo must provide CARGO_PKG_DESCRIPTION to build scripts");
    assert_resource_string("CARGO_PKG_AUTHORS", &package_authors);
    assert_resource_string("CARGO_PKG_DESCRIPTION", &package_description);
    let numeric_version = package_version
        .split_once('-')
        .map_or(package_version.as_str(), |(version, _)| version);
    let mut components = numeric_version.split('.');
    let major = components.next().unwrap_or("0");
    let minor = components.next().unwrap_or("0");
    let patch = components.next().unwrap_or("0");
    assert!(
        components.next().is_none()
            && [major, minor, patch]
                .iter()
                .all(|component| component.parse::<u16>().is_ok()),
        "CARGO_PKG_VERSION must contain three 16-bit numeric components"
    );
    fs::write(
        &version_header,
        format!(
            "#define PIERDESK_VERSION {major},{minor},{patch},0\n\
             #define PIERDESK_VERSION_STRING \"{major}.{minor}.{patch}.0\\0\"\n\
             #define PIERDESK_AUTHOR_STRING \"{package_authors}\\0\"\n\
             #define PIERDESK_DESCRIPTION_STRING \"{package_description}\\0\"\n"
        ),
    )
    .expect("failed to write the generated Windows version header");

    let status = Command::new(&resource_compiler)
        .current_dir(&manifest_directory)
        .arg("/nologo")
        .arg(format!("/I{}", output_directory.display()))
        .arg(format!("/fo{}", compiled_resource.display()))
        .arg(&resource_source)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "failed to launch the Windows resource compiler {}: {error}",
                PathBuf::from(&resource_compiler).display()
            )
        });
    assert!(status.success(), "Windows resource compilation failed");

    println!(
        "cargo:rustc-link-arg-bin=pierdesk={}",
        compiled_resource.display()
    );
}

fn assert_resource_string(name: &str, value: &str) {
    assert!(
        !value.is_empty()
            && !value
                .chars()
                .any(|character| matches!(character, '\0' | '\r' | '\n' | '"' | '\\')),
        "{name} must be a non-empty single-line Windows resource string without quotes or slashes"
    );
}
