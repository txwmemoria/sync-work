//! Pairing two machines.
//!
//! Three commands, run once ever: `init` on one machine, `join` on the other,
//! `join` back on the first. A ticket carries only a public key, a name, and a
//! platform — no paths and no project names — which is why one pairing then covers
//! every project the two machines share.

use crate::cli::PairCmd;
use crate::config::{Identity, Machine, Platform};
use crate::style::Paint;
use crate::transport;
use anyhow::{Result, bail};

pub async fn run(cmd: PairCmd) -> Result<i32> {
    let mut machine = Machine::load()?;
    let identity = Identity::load_or_create()?;

    match cmd {
        PairCmd::Init { name } => {
            let ticket = transport::with_endpoint(&identity, |ep| {
                let machine = &machine;
                async move { transport::local_ticket(&ep, machine, name, None).await }
            })
            .await?;

            // Remember whatever name we just published, so the peer's view and
            // ours agree from here on.
            machine.node.name = Some(ticket.name.clone());
            machine.save()?;

            println!("{}", "Run this on the other machine:".bold());
            println!();
            println!("  sw pair join {}", ticket.encode()?);
            println!();
            println!("{}", "It will print a ticket to bring back here.".dimmed());
            Ok(0)
        }

        PairCmd::Join { ticket, name } => {
            let incoming = transport::Ticket::decode(&ticket)?;
            if incoming.id == identity.id_string() {
                bail!("that ticket is from this machine");
            }

            let acking = Some(incoming.id.clone());
            let mine = transport::with_endpoint(&identity, |ep| {
                let machine = &machine;
                async move { transport::local_ticket(&ep, machine, name, acking).await }
            })
            .await?;

            // Peer names are table keys on both sides; two machines sharing one
            // would make the pairing ambiguous.
            if incoming.name == mine.name {
                bail!(
                    "both machines are called `{}`; re-run with --name to tell them apart",
                    mine.name
                );
            }

            machine
                .peers
                .insert(incoming.name.clone(), incoming.as_peer());
            machine.node.name = Some(mine.name.clone());
            machine.save()?;

            println!(
                "{} {} ({})",
                "paired with".green().bold(),
                incoming.name,
                incoming.platform
            );
            // If the incoming ticket acknowledges us, the other machine already
            // has our key and the pairing is mutual — prompting for another round
            // trip would send the user off to run a no-op.
            if incoming.acking.as_deref() == Some(identity.id_string().as_str()) {
                println!();
                println!(
                    "{} both machines now know each other.",
                    "done".green().bold()
                );
                println!(
                    "Next, in each checkout you want to verify: {}",
                    "sw link".cyan()
                );
            } else {
                println!();
                println!("{}", "Run this back on the other machine:".bold());
                println!();
                println!("  sw pair join {}", mine.encode()?);
            }
            Ok(0)
        }

        PairCmd::List => {
            println!("{} {}", "this machine".bold(), machine.node_name());
            println!("  id       {:.16}…", identity.id_string());
            println!("  platform {}", Platform::current());
            // Printed because a key mismatch is otherwise a filesystem hunt, and
            // because this path is not always what the docs imply: on Windows an
            // app container can redirect it, so two shells on the same machine
            // may read different state.
            match crate::config::machine::home() {
                Ok(p) => println!("  state    {}", p.display()),
                Err(e) => println!("  state    {} {e:#}", "unknown:".red()),
            }
            if machine.peers.is_empty() {
                println!("\nno peers; run `sw pair init`");
                return Ok(0);
            }
            for (name, p) in &machine.peers {
                println!("\n{}", name.bold());
                println!("  id       {:.16}…", p.id);
                println!("  platform {}", p.platform);
            }
            Ok(0)
        }
    }
}
