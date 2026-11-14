//! Headless daemon entry point (scaffold).

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("bt-daemon scaffold on {:?}", bt_platform::current_os());
    Ok(())
}
