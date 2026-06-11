mod deploy;
mod hosting;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use hosting::StaticHost;
use pingora::apps::http_app::HttpServer;
use pingora::prelude::*;
use pingora::services::listening::Service;

#[derive(Debug, Parser)]
#[command(name = "quick", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Copy a static site into the local site store.
    Deploy {
        /// Directory containing index.html and other static assets.
        source: PathBuf,
        /// Site name, used as the subdomain.
        #[arg(short, long)]
        site: String,
        /// Directory where deployed sites are stored.
        #[arg(long, default_value = "sites")]
        sites_dir: PathBuf,
    },
    /// Serve all deployed sites with Pingora.
    Serve {
        /// TCP address on which the HTTP server listens.
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
        /// Parent directory containing one directory per site.
        #[arg(long, default_value = "sites")]
        sites_dir: PathBuf,
        /// Base domain stripped from the Host header.
        #[arg(long, default_value = "localhost")]
        base_domain: String,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Deploy {
            source,
            site,
            sites_dir,
        } => {
            if let Err(error) = deploy::deploy(&source, &sites_dir, &site) {
                eprintln!("deploy failed: {error}");
                std::process::exit(1);
            }

            println!("deployed {site} from {}", source.display());
        }
        Command::Serve {
            listen,
            sites_dir,
            base_domain,
        } => run_server(listen, sites_dir, base_domain),
    }
}

fn run_server(listen: String, sites_dir: PathBuf, base_domain: String) {
    let mut server = Server::new(None).expect("failed to create Pingora server");
    server.bootstrap();

    let app = StaticHost::new(sites_dir, base_domain);
    let http_server = HttpServer::new_app(app);
    let mut service = Service::new("Quick static hosting".to_owned(), http_server);
    service.add_tcp(&listen);
    server.add_service(service);

    println!("quick serving on http://{listen}");
    server.run_forever();
}
