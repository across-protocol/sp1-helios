use sp1_build::{build_program_with_args, BuildArgs};

fn main() {
    build_program_with_args(
        "../program",
        BuildArgs {
            // The SP1 v6.0.2 Docker image is x86_64-only, so docker builds don't work on ARM Macs.
            // Reproducible ELF builds are verified in CI via elf.yml (--docker --tag v6.0.2).
            docker: true,
            tag: "v6.0.2".into(),
            elf_name: Some("sp1-helios-elf".to_string()),
            output_directory: Some("../elf".to_string()),
            ..Default::default()
        },
    );
}
