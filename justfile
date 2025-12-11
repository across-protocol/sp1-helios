clean:
    rm -rf target

# This build command uses cargo prove binary (updated via `sp1up`) to generate the elf file. This
# uses pre-packaged sp1-build distribution under the hood. Therefore, to get a reproducible build
# compared to `zk-api/build.rs`, we should pin the exact verion of the docker image by using `--tag`
# We default to using a version of `sp1-build` from Cargo.toml
update-elf:
    cd program && cargo prove build --elf-name sp1-helios-elf --docker --tag v5.2.1 --output-directory ../elf