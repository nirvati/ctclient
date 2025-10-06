use ctclient_async::{CTClient, certutils, google_log_list::LogList};
use openssl::x509::X509;
use std::io::Write;

#[tokio::main]
async fn main() {
    env_logger::init();

    if std::env::args_os().len() != 1 {
        eprintln!("Expected no arguments.");
        std::process::exit(1);
    }

    let all_certs = LogList::get().await.expect("Failed to get log list");
    for (_id, log) in all_certs.map_id_to_log {
        tokio::spawn(async move {
            // URL and public key copy-pasted from https://www.gstatic.com/ct/log_list/v3/all_logs_list.json .
            // Google's CT log updates very quickly so we use it here.
            let mut client = match CTClient::new_from_latest_th(&log.base_url, &log.pub_key).await {
                Ok(client) => client,
                Err(e) => {
                    log::error!("{:?}", e);
                    return;
                }
            };
            loop {
                let update_result = client
                    .update(Some(|certs: &[X509]| {
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
                    }))
                    .await;
                if update_result.is_err() {
                    eprintln!("Error: {}", update_result.unwrap_err());
                }
                std::io::stdout().flush().unwrap();
            }
        });
    }
}
