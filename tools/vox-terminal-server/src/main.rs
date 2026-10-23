#[tokio::main]
async fn main() -> eyre::Result<()> {
    let mut listen = String::from("127.0.0.1:0");
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--listen" => {
                listen = arguments
                    .next()
                    .ok_or_else(|| eyre::eyre!("--listen requires an address"))?
            }
            "--help" | "-h" => {
                println!("teamy-terminal-vox-server [--listen HOST:PORT]");
                return Ok(());
            }
            other => return Err(eyre::eyre!("unknown argument: {other}")),
        }
    }
    teamy_terminal_cli::serve_vox(&listen).await
}
