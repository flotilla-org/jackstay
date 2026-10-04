//! Run `cargo run -p jackstay-producer --example navigation` then attach using
//! bootstrap::connect_v2. Source selection and authorization remain host policy.
use std::time::Duration;

use jackstay::{
    acquisition::arena::{ArenaConfig, FrameDescriptor},
    affordances::{Domain, Event, Navigation, Size, Snapshot, Window},
    input::{Config, Operation, Outcome, Work},
    local::{Endpoint, Scope, Transport},
    model::PixelFormat,
};
use jackstay_producer::{Builder, Frame, Producer};
struct Content {
    width: u32,
    height: u32,
    announced: bool,
    dirty: bool,
    index: usize,
    loading: bool,
    url: String,
}
impl Producer for Content {
    fn frame(&mut self) -> Option<Frame> {
        // Synthetic pattern changes with the navigation history index.
        let mut bytes = Vec::with_capacity((self.width * self.height * 4) as usize);
        for y in 0..self.height {
            for x in 0..self.width {
                let value = if (x + y + self.index as u32) % 40 < 20 { 220 } else { 40 };
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
        match work.operation {
            Operation::Cleanup { .. } => Outcome::Executed,
            Operation::Event(_) => {
                eprintln!("unexpected pointer/key input");
                Outcome::Unsupported
            }
        }
    }
    fn snapshots(&mut self) -> Vec<Snapshot> {
        let mut snapshots = Vec::new();
        if !self.announced {
            self.announced = true;
            snapshots.push(Snapshot::Window(Window {
                title: Some("Navigation producer".into()),
                requested_size: Some(Size { width: 640., height: 480. }),
                ready: true,
            }));
        }
        if self.dirty {
            self.dirty = false;
            snapshots.push(Snapshot::Navigation(Navigation {
                url: Some(self.url.clone()),
                title: None,
                // This synthetic history has exactly two entries: indices 0 and 1.
                can_go_back: self.index > 0,
                can_go_forward: self.index == 0,
                loading: self.loading,
                capabilities: ["back", "forward", "reload", "stop", "load"]
                    .into_iter()
                    .map(|name| (name.into(), true))
                    .collect(),
            }));
        }
        snapshots
    }
    fn affordance(&mut self, event: Event) {
        if let Event::Verb(ref verb) = event {
            if verb.domain == Domain::Navigation {
                match verb.name.as_str() {
                    "back" => self.index = 0,
                    "forward" => self.index = 1,
                    "reload" => self.loading = true,
                    "stop" => self.loading = false,
                    "load" => {
                        let Some(url) = verb.body["url"].as_str() else { return };
                        self.url = url.into();
                    }
                    _ => return,
                }
                self.dirty = true;
                eprintln!("navigation verb={} url={}", verb.name, self.url);
            }
        }
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
    let endpoint = Endpoint::new(Scope::User, "navigation-producer", Transport::LocalStream)?;
    // The toolkit reconfigures payload capacity on demand when presentation
    // grows the frame; the 2048-pixel clamp keeps each frame at most 16 MiB.
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
        Config { ..Config::default() },
        Content {
            width: 640,
            height: 480,
            announced: false,
            dirty: true,
            index: 1,
            loading: false,
            url: "https://example.test/current".into(),
        },
    )
    .start()?;
    std::thread::sleep(Duration::from_secs(10));
    source.stop()?;
    Ok(())
}
