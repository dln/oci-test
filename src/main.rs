use oci_distribution::manifest;
use oci_distribution::client::*;
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::Reference;
use tracing_subscriber::prelude::*;
use std::io::Cursor;
use flate2::read::GzDecoder;
use tar::Archive;

#[tracing::instrument()]
async fn pull_image(image: &str) -> Result<ImageData, oci_distribution::errors::OciDistributionError> {
    let reference = Reference::try_from(image).unwrap();
    let auth = RegistryAuth::Anonymous;
    let config = ClientConfig {
        protocol: ClientProtocol::Https,
        ..Default::default()
    };
    let mut client = Client::new(config);

    tracing::info!("Pulling image: {:?}", image);
    let types = vec![
        manifest::IMAGE_LAYER_MEDIA_TYPE, // OCI
        manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
        manifest::IMAGE_DOCKER_LAYER_TAR_MEDIA_TYPE, // Docker
        manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
    ];
    return client.pull(&reference, &auth, types).await;
}

#[tracing::instrument(skip(image_data))]
async fn unpack_image(image_data: oci_distribution::client::ImageData) -> std::io::Result<()> {
    tracing::info!("Unpacking image");
    for layer in image_data.layers {
        let tar_gz = Cursor::new(layer.data);
        let tar = GzDecoder::new(tar_gz);
        let mut archive = Archive::new(tar);
        archive.unpack("/tmp/image")?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .init();

    let image_data = pull_image("docker.io/library/alpine:latest").await?;
    unpack_image(image_data).await?;
    Ok(())
}

