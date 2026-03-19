use anyhow::Result;
use sp1_sdk::{
    blocking::{MockProver, Prover},
    Elf, HashableKey, ProvingKey,
};

const HELIOS_ELF: &[u8] = include_bytes!("../../elf/sp1-helios-elf");

fn main() -> Result<()> {
    let client = MockProver::new();
    let pk = client.setup(Elf::Static(HELIOS_ELF))?;
    println!(
        "SP1 Helios Verifying Key: {:?}",
        pk.verifying_key().bytes32()
    );
    Ok(())
}
