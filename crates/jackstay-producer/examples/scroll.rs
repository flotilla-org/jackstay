//! Run `cargo run -p jackstay-producer --example scroll` then attach using
//! bootstrap::connect_v2. Source selection and authorization remain host policy.
use std::time::Duration;

use jackstay::{
    acquisition::arena::{ArenaConfig, FrameDescriptor},
    affordances::{Axis, Domain, Event, Scroll, Size, Snapshot, Window},
    input::{Config, Event as InputEvent, Operation, Outcome, ScrollUnit, Work},
    local::{Endpoint, Scope, Transport},
    model::PixelFormat,
};
use jackstay_producer::{Builder, Frame, Producer};
struct Content {
    width: u32,
    height: u32,
    announced: bool,
    dirty: bool,
    read_only: bool,
    x: f64,
    y: f64,
}
impl Producer for Content {
    fn frame(&mut self) -> Option<Frame> {
        // One-pixel checkerboard reveals any scaling blur on HiDPI displays.
        let mut bytes = Vec::with_capacity((self.width * self.height * 4) as usize);
        for y in 0..self.height {
            for x in 0..self.width {
                let value = if (x + y + self.x as u32 + self.y as u32) % 40 < 20 {
                    220
                } else {
                    40
                };
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
            Operation::Event(InputEvent::Scroll { x, y, unit, .. }) => {
                let factor = match unit {
                    ScrollUnit::Pixel => 1.,
                    ScrollUnit::Line => 20.,
                    ScrollUnit::Page => 200.,
                };
                self.x = (self.x + x * factor).clamp(0., 800.);
                self.y = (self.y + y * factor).clamp(0., 800.);
                self.dirty = true;
                eprintln!("wheel x={} y={}", self.x, self.y);
                Outcome::Executed
            }
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
                title: Some("Scroll producer".into()),
                requested_size: Some(Size { width: 640., height: 480. }),
                ready: true,
            }));
        }
        if self.dirty {
            self.dirty = false;
            snapshots.push(Snapshot::Scroll(Scroll {
                x: Axis {
                    scrollable: true,
                    content_length: 1000.,
                    viewport_length: 200.,
                    position: self.x,
                },
                y: Axis {
                    scrollable: true,
                    content_length: 1000.,
                    viewport_length: 200.,
                    position: self.y,
                },
                capabilities: [("set_position".into(), !self.read_only), ("scroll_by_step".into(), !self.read_only)].into(),
            }));
        }
        snapshots
    }
    fn affordance(&mut self, event: Event) {
        if let Event::Verb(ref verb) = event {
            if verb.domain == Domain::Scroll {
                let position = if verb.body["axis"] == "x" { &mut self.x } else { &mut self.y };
                match verb.name.as_str() {
                    "set_position" => *position = verb.body["position"].as_f64().unwrap().clamp(0., 800.),
                    "scroll_by_step" => {
                        let step = if verb.body["step"] == "large" { 200. } else { 20. };
                        let direction = if verb.body["direction"] == "increment" { 1. } else { -1. };
                        *position = (*position + step * direction).clamp(0., 800.);
                    }
                    _ => return,
                }
                self.dirty = true;
                eprintln!("verb={} axis={} position={}", verb.name, verb.body["axis"], position);
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
    let endpoint = Endpoint::new(Scope::User, "scroll-producer", Transport::LocalStream)?;
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
            read_only: std::env::args().any(|arg| arg == "--read-only"),
            x: 50.,
            y: 100.,
        },
    )
    .start()?;
    std::thread::sleep(Duration::from_secs(10));
    source.stop()?;
    Ok(())
}
