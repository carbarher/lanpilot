fn main() {
    #[cfg(windows)]
    {
        let mut resource = winres::WindowsResource::new();
        resource.set_icon("assets\\lanpilot.ico");
        resource
            .compile()
            .expect("failed to compile Windows icon resources");
    }
}
