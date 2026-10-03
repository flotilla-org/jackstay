//! Run `cargo run -p jackstay-producer --example minimal` then attach using
//! bootstrap::connect_v2. Source selection and authorization remain host policy.
use std::time::Duration;

use jackstay::{
    acquisition::arena::{ArenaConfig, FrameDescriptor},
    affordances::{Snapshot, Window},
    input::{Config, Operation, Outcome, Work},
    local::{Endpoint, Scope, Transport},
    model::PixelFormat,
};
use jackstay_producer::{Builder, Frame, Producer};
struct Content;
impl Producer for Content {
    fn frame(&mut self) -> Option<Frame> {
        Some(Frame {
            descriptor: FrameDescriptor {
                width: 1,
                height: 1,
                stride: 4,
                pixel_format: PixelFormat::Rgba8Unorm as u32,
                ..Default::default()
            },
            bytes: vec![255, 0, 0, 255],
        })
    }
    fn execute(&mut self, work: Work) -> Outcome {
        // This example never executes input, so it has no held state to release.
        match work.operation {
            Operation::Cleanup { .. } => Outcome::Executed,
            Operation::Event(_) => Outcome::Unsupported,
        }
    }
    fn snapshots(&mut self) -> Vec<Snapshot> {
        vec![Snapshot::Window(Window {
            title: Some("Minimal producer".into()),
            requested_size: None,
            ready: true,
        })]
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = Endpoint::new(Scope::User, "minimal-producer", Transport::LocalStream)?;
    let arena = ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations: 4,
        drain_timeout: Duration::from_secs(5),
    };
    let source = Builder::new(
        endpoint,
        arena,
        Config {
            capabilities: 0,
            ..Config::default()
        },
        Content,
    )
    .start()?;
    std::thread::sleep(Duration::from_secs(10));
    source.stop()?;
    Ok(())
}
