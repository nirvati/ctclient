use ctclient::{certutils, CTClient};
use openssl::x509::X509;
use std::io::Write;

fn main() {
    env_logger::init();

    if std::env::args_os().len() != 1 {
        eprintln!("Expected no arguments.");
        std::process::exit(1);
    }

    // URL and public key copy-pasted from https://www.gstatic.com/ct/log_list/v2/all_logs_list.json .
    // Google's CT log updates very quickly so we use it here.
    let public_key = base64::decode("MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEr+TzlCzfpie1/rJhgxnIITojqKk9VK+8MZoc08HjtsLzD8e5yjsdeWVhIiWCVk6Y6KomKTYeKGBv6xVu93zQug==").unwrap();
    const URL: &str = "https://ct.googleapis.com/logs/us1/argon2025h2/";
    let mut client = CTClient::new_from_latest_th(URL, &public_key).unwrap();
    loop {
        let update_result = client.update(Some(|certs: &[X509]| {
            let leaf = &certs[0];
            let ca = &certs[1];
            let canames = certutils::get_common_names(ca).unwrap();
            let caname = &canames[0];
            if let Ok(domains) = certutils::get_dns_names(leaf) {
                print!("{}: ", caname);
                let mut first = true;
                for d in domains.into_iter() {
                    if !first {
                        print!(", ");
                    }
                    print!("{}", d);
                    first = false;
                }
                print!("\n");
            }
        }));
        if update_result.is_err() {
            eprintln!("Error: {}", update_result.unwrap_err());
        }
        std::io::stdout().flush().unwrap();
    }
}
