mod deploy;
mod hosting;
mod storage;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use hosting::StaticHost;
use pingora::apps::http_app::HttpServer;
use pingora::prelude::*;
use pingora::services::listening::Service;
use storage::{LocalStorage, S3Storage, SharedStorage};

#[derive(Debug, Parser)]
#[command(name = "quick", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Deploy a static site to the configured storage backend.
    Deploy {
        /// Directory containing the files to publish.
        source: PathBuf,
        /// Site name, used as the subdomain.
        #[arg(short, long)]
        site: String,
        #[command(flatten)]
        storage: StorageOptions,
    },
    /// Serve all deployed sites with Pingora.
    Serve {
        /// TCP address on which the HTTP server listens.
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
        /// Base domain stripped from the Host header.
        #[arg(long, default_value = "localhost")]
        base_domain: String,
        #[command(flatten)]
        storage: StorageOptions,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum StorageKind {
    Local,
    S3,
}

#[derive(Clone, Debug, Args)]
struct StorageOptions {
    /// Storage backend used for deployments and hosted assets.
    #[arg(long, value_enum, default_value = "local", env = "QUICK_STORAGE")]
    storage: StorageKind,
    /// Parent directory containing one directory per site in local mode.
    #[arg(long, default_value = "sites", env = "QUICK_SITES_DIR")]
    sites_dir: PathBuf,
    /// S3 bucket name.
    #[arg(long, env = "QUICK_S3_BUCKET")]
    s3_bucket: Option<String>,
    /// S3 region or signing region for compatible services.
    #[arg(long, default_value = "us-east-1", env = "QUICK_S3_REGION")]
    s3_region: String,
    /// Custom S3-compatible endpoint, for example MinIO or Cloudflare R2.
    #[arg(long, env = "QUICK_S3_ENDPOINT")]
    s3_endpoint: Option<String>,
    /// Prefix under which Quick stores all objects.
    #[arg(long, default_value = "quick", env = "QUICK_S3_PREFIX")]
    s3_prefix: String,
    /// Use path-style S3 URLs. Usually required by MinIO.
    #[arg(long, env = "QUICK_S3_PATH_STYLE")]
    s3_path_style: bool,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Deploy {
            source,
            site,
            storage,
        } => {
            if let Err(error) = deploy_command(&source, &site, storage) {
                eprintln!("deploy failed: {error}");
                std::process::exit(1);
            }

            println!("deployed {site} from {}", source.display());
        }
        Command::Serve {
            listen,
            base_domain,
            storage,
        } => {
            let storage = build_storage(&storage).unwrap_or_else(|error| {
                eprintln!("storage configuration failed: {error}");
                std::process::exit(1);
            });
            run_server(listen, base_domain, storage);
        }
    }
}

fn deploy_command(source: &std::path::Path, site: &str, options: StorageOptions) -> io::Result<()> {
    match options.storage {
        StorageKind::Local => deploy::deploy(source, &options.sites_dir, site),
        StorageKind::S3 => {
            let storage = build_storage(&options)?;
            let files = deploy::read_directory(source)?;
            storage::block_on(storage.deploy(site, files))?
        }
    }
}

fn build_storage(options: &StorageOptions) -> io::Result<SharedStorage> {
    match options.storage {
        StorageKind::Local => Ok(Arc::new(LocalStorage::new(options.sites_dir.clone()))),
        StorageKind::S3 => {
            let bucket = options.s3_bucket.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--s3-bucket is required when --storage s3",
                )
            })?;
            Ok(Arc::new(S3Storage::new(
                bucket,
                &options.s3_region,
                options.s3_endpoint.as_deref(),
                &options.s3_prefix,
                options.s3_path_style,
            )?))
        }
    }
}

fn run_server(listen: String, base_domain: String, storage: SharedStorage) {
    let mut server = Server::new(None).expect("failed to create Pingora server");
    server.bootstrap();

    let storage_description = storage.description();
    let app = StaticHost::new(storage, base_domain);
    let http_server = HttpServer::new_app(app);
    let mut service = Service::new("Quick static hosting".to_owned(), http_server);
    service.add_tcp(&listen);
    server.add_service(service);

    println!("quick serving on http://{listen} using {storage_description}");
    server.run_forever();
}
