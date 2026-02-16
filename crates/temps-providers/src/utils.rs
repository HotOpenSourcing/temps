use bollard::{models::NetworkCreateRequest, query_parameters::ListNetworksOptions, Docker};
use std::collections::HashMap;
use tracing::{error, info};

pub(crate) async fn ensure_network_exists(
    docker: &Docker,
) -> Result<(), Box<dyn std::error::Error>> {
    let network_name = temps_core::NETWORK_NAME.as_str();

    // Check if network exists
    let networks = docker.list_networks(None::<ListNetworksOptions>).await?;
    let network_exists = networks
        .iter()
        .any(|n| n.name.as_deref() == Some(network_name));

    if !network_exists {
        info!("Creating network: {}", network_name);
        let options = NetworkCreateRequest {
            name: network_name.to_string(),
            driver: Some("bridge".to_string()),
            ..Default::default()
        };

        match docker.create_network(options).await {
            Ok(_) => info!("Successfully created network: {}", network_name),
            Err(e) => {
                error!("Failed to create network: {}", e);
                return Err(Box::new(e));
            }
        }
    }

    Ok(())
}

/// Create a Docker log configuration for external service containers.
/// Uses `json-file` driver with configurable size limits to prevent unbounded log growth.
///
/// Default: 20MB max per file, 3 rotated files = 60MB max total per container.
pub(crate) fn service_log_config(
    max_size: &str,
    max_file: u32,
) -> bollard::models::HostConfigLogConfig {
    let mut config = HashMap::new();
    config.insert("max-size".to_string(), max_size.to_string());
    config.insert("max-file".to_string(), max_file.to_string());

    bollard::models::HostConfigLogConfig {
        typ: Some("json-file".to_string()),
        config: Some(config),
    }
}

/// Create default Docker log configuration for external service containers.
/// 20MB max per file, 3 rotated files = 60MB max total.
pub(crate) fn default_service_log_config() -> bollard::models::HostConfigLogConfig {
    service_log_config("20m", 3)
}
