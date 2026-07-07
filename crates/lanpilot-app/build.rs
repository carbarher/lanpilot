fn main() {
    #[cfg(windows)]
    {
        let mut resource = winres::WindowsResource::new();
        resource.set_icon("assets\\lanpilot.ico");
        
        let is_release = std::env::var("PROFILE").map(|v| v == "release").unwrap_or(false);
        if is_release {
            resource.set_manifest(r#"
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
<trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
        <requestedPrivileges>
            <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
        </requestedPrivileges>
    </security>
</trustInfo>
</assembly>
"#);
        }

        resource
            .compile()
            .expect("failed to compile Windows icon resources");
    }
}
