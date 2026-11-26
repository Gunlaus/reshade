fn main() {
    let configuration = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/manager.slint", configuration)
        .expect("compile the manager interface");
}
