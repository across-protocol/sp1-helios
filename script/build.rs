use sp1_build::{build_program_with_args, BuildArgs};

fn main() {
    build_program_with_args(
        "../program",
        BuildArgs {
            // We use `docker: true` for reproducible builds and tag to the latest included sp1-build
            // dependency from `Cargo.toml`. https://docs.succinct.xyz/docs/sp1/writing-programs/compiling
            docker: false,
            tag: "v5.2.1".into(),
            elf_name: Some("sp1-helios-elf".to_string()),
            output_directory: Some("../elf".to_string()),
            ..Default::default()
        },
    );
}
