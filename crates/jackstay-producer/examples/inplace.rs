//! A host can fill source arena slots or export them to its trusted child.
use std::time::Duration;

use jackstay::{
    acquisition::arena::{ArenaConfig, FrameDescriptor},
    input::{Config, Outcome, Work},
    local::{Endpoint, Scope, Transport},
};
use jackstay_producer::{Builder, Frame, Producer};
struct Idle;
impl Producer for Idle {
    fn frame(&mut self) -> Option<Frame> {
        None
    }
    fn execute(&mut self, _: Work) -> Outcome {
        Outcome::Executed
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = Endpoint::new(Scope::User, "inplace-example", Transport::LocalStream)?;
    let source = Builder::new(
        endpoint,
        ArenaConfig {
            resource_capacity: 4,
            retained_history: 1,
            producer_reserve: 1,
            payload_capacity: 16,
            memory_budget: 1 << 20,
            max_incarnations: 1,
            drain_timeout: Duration::from_secs(5),
        },
        Config::default(),
        Idle,
    )
    .start()?;
    source.with_arena(|arena| -> Result<(), jackstay::acquisition::arena::ArenaError> {
        if let Some(mut slot) = arena.reserve()? {
            slot.bytes_mut().copy_from_slice(&[255; 16]);
            arena.commit(
                slot,
                FrameDescriptor {
                    payload_len: 16,
                    width: 4,
                    height: 1,
                    stride: 16,
                    ..Default::default()
                },
            )?;
        }
        // arena.export_writer() retains the allocation for a trusted child's
        // writable payload mapping. Retain until child confirms unmap or exits.
        Ok(())
    })?;
    source.stop()?;
    Ok(())
}
