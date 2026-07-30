use std::path::PathBuf;
use std::process::Command;

#[test]
fn public_header_compiles_as_strict_c11() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let temporary = tempfile::tempdir().unwrap();
    let source = root.join("tests/fixtures/c_api_consumer.c");
    let include = root.join("include");
    let status = if cfg!(target_env = "msvc") {
        Command::new("cl")
            .arg("/nologo")
            .arg("/std:c11")
            .arg("/W4")
            .arg("/WX")
            .arg(format!("/I{}", include.display()))
            .arg("/c")
            .arg(source)
            .arg(format!(
                "/Fo{}",
                temporary.path().join("consumer.obj").display()
            ))
            .status()
    } else {
        Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .arg("-std=c11")
            .arg("-Wall")
            .arg("-Wextra")
            .arg("-Werror")
            .arg("-I")
            .arg(include)
            .arg("-c")
            .arg(source)
            .arg("-o")
            .arg(temporary.path().join("consumer.o"))
            .status()
    }
    .expect("run the platform C compiler");
    assert!(status.success());
}

#[test]
fn public_header_compiles_as_cpp17() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let temporary = tempfile::tempdir().unwrap();
    let source = root.join("tests/fixtures/c_api_consumer.c");
    let include = root.join("include");
    let status = if cfg!(target_env = "msvc") {
        Command::new("cl")
            .arg("/nologo")
            .arg("/TP")
            .arg("/std:c++17")
            .arg("/W4")
            .arg("/WX")
            .arg(format!("/I{}", include.display()))
            .arg("/c")
            .arg(source)
            .arg(format!(
                "/Fo{}",
                temporary.path().join("consumer-cpp.obj").display()
            ))
            .status()
    } else {
        Command::new(std::env::var_os("CXX").unwrap_or_else(|| "c++".into()))
            .arg("-x")
            .arg("c++")
            .arg("-std=c++17")
            .arg("-Wall")
            .arg("-Wextra")
            .arg("-Werror")
            .arg("-I")
            .arg(include)
            .arg("-c")
            .arg(source)
            .arg("-o")
            .arg(temporary.path().join("consumer-cpp.o"))
            .status()
    }
    .expect("run the platform C++ compiler");
    assert!(status.success());
}
