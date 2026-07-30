fn main() {
    #[cfg(windows)]
    {
        let icon = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/resources/windows/app-icon.ico"
        ));
        windows_resources::compile_with_icon(false, icon, "som-key")
            .expect("failed to compile Windows resources");
    }
}
