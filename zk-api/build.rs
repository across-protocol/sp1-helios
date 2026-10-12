use sp1_build::{build_program_with_args, BuildArgs};

fn main() {
    build_program_with_args(
        "../program",
        BuildArgs {
            // We use `docker: true` for reproducible builds and tag to the latest included sp1-build
            // dependency from `Cargo.toml`. https://docs.succinct.xyz/docs/sp1/writing-programs/compiling
            docker: true,
            tag: "v6.8.1".into(),
            // Build from the committed Cargo.lock so the container cannot re-resolve deps.
            locked: true,
            elf_name: Some("sp1-helios-elf".to_string()),
            output_directory: Some("../elf".to_string()),
            ..Default::default()
        },
    );
}
