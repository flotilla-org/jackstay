//! Run `cargo run -p jackstay-producer --example minimal` then attach using
//! bootstrap::connect_v2. Source selection and authorization remain host policy.
use std::time::Duration;

use jackstay::{
    acquisition::arena::{ArenaConfig, FrameDescriptor},
    affordances::{Event, Size, Snapshot, Window},
    input::{Config, Operation, Outcome, Work},
    local::{Endpoint, Scope, Transport},
    model::PixelFormat,
};
use jackstay_producer::{Builder, Frame, Producer};
struct Content {
    width: u32,
    height: u32,
    announced: bool,
}
impl Producer for Content {
    fn frame(&mut self) -> Option<Frame> {
        // One-pixel checkerboard reveals any scaling blur on HiDPI displays.
        let mut bytes = Vec::with_capacity((self.width * self.height * 4) as usize);
        for y in 0..self.height {
            for x in 0..self.width {
                let value = if (x + y) % 2 == 0 { 255 } else { 0 };
                bytes.extend_from_slice(&[value, value, value, 255]);
            }
        }
        Some(Frame {
            descriptor: FrameDescriptor {
                width: self.width,
                height: self.height,
                stride: self.width * 4,
                pixel_format: PixelFormat::Rgba8Unorm as u32,
                ..Default::default()
            },
            bytes,
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
        if self.announced {
            return Vec::new();
        }
        self.announced = true;
        vec![Snapshot::Window(Window {
            title: Some("Minimal producer".into()),
            requested_size: Some(Size { width: 640., height: 480. }),
            ready: true,
        })]
    }
    fn affordance(&mut self, event: Event) {
        if let Event::Snapshot(Snapshot::Presentation(p)) = event {
            if let Some(size) = p.preferred_size {
                self.width = (size.width * p.scale).round().clamp(1., 2048.) as u32;
                self.height = (size.height * p.scale).round().clamp(1., 2048.) as u32;
                eprintln!("presentation frame={}x{} scale={}", self.width, self.height, p.scale);
            }
        }
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = Endpoint::new(Scope::User, "minimal-producer", Transport::LocalStream)?;
    let arena = ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 640 * 480 * 4,
        memory_budget: 256 * 1024 * 1024,
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
        Content {
            width: 640,
            height: 480,
            announced: false,
        },
    )
    .start()?;
    std::thread::sleep(Duration::from_secs(10));
    source.stop()?;
    Ok(())
}
