use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;

use clipferry_license_protocol::{generate_device_key, public_key};
use clipferry_license_server::LicenseService;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or(
        "usage: server init|public-key|create-codes|lookup|unbind|disable|serve DATA_DIR [ARG]",
    )?;
    let data_dir = PathBuf::from(args.next().ok_or("DATA_DIR required")?);
    let key_path = data_dir.join("signing-key.pk8");
    if command == "init" {
        if args.next().is_some() {
            return Err("unexpected argument".into());
        }
        fs::create_dir_all(&data_dir)?;
        let signing_key = generate_device_key()?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(key_path)?;
        file.write_all(&signing_key)?;
        file.sync_all()?;
        println!("issuer_public_key={}", public_key(&signing_key)?);
        return Ok(());
    }
    let signing_key = fs::read(key_path)?;
    if command == "public-key" {
        if args.next().is_some() {
            return Err("unexpected argument".into());
        }
        println!("issuer_public_key={}", public_key(&signing_key)?);
        return Ok(());
    }
    let service = LicenseService::open(&data_dir.join("licenses.sqlite"), signing_key)?;
    match command.as_str() {
        "create-codes" => {
            let count: usize = args.next().ok_or("COUNT required")?.parse()?;
            if args.next().is_some() {
                return Err("unexpected argument".into());
            }
            for code in service.create_codes(count)? {
                println!("{code}");
            }
        }
        "lookup" => {
            let code = args.next().ok_or("CODE required")?;
            if args.next().is_some() {
                return Err("unexpected argument".into());
            }
            println!(
                "{}",
                service
                    .license_id_for_code(&code)?
                    .ok_or("code not found")?
            );
        }
        "unbind" => {
            let id = args.next().ok_or("LICENSE_ID required")?;
            if args.next().is_some() {
                return Err("unexpected argument".into());
            }
            if !service.unbind(&id)? {
                return Err("license not found".into());
            }
            println!("unbound");
        }
        "disable" => {
            let id = args.next().ok_or("LICENSE_ID required")?;
            if args.next().is_some() {
                return Err("unexpected argument".into());
            }
            if !service.disable(&id)? {
                return Err("license not found or already disabled".into());
            }
            println!("disabled");
        }
        "serve" => {
            let address: SocketAddr = args
                .next()
                .unwrap_or_else(|| "127.0.0.1:8787".to_owned())
                .parse()?;
            if !address.ip().is_loopback() || args.next().is_some() {
                return Err("server must bind loopback only".into());
            }
            let listener = tokio::net::TcpListener::bind(address).await?;
            println!("listening={address}");
            axum::serve(listener, service.router()).await?;
        }
        _ => return Err("unknown command".into()),
    }
    Ok(())
}
