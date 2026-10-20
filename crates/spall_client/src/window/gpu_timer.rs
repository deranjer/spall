use super::*;

pub(super) struct HudGpuReadback {
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    receiver: Option<mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>>,
}

/// Optional GPU timestamp around the Yakui pass. Readbacks are asynchronous
/// and rotate through three buffers so measurement never stalls presentation.
pub(super) struct HudGpuTimer {
    queries: wgpu::QuerySet,
    slots: [HudGpuReadback; 3],
    period_ns: f32,
    latest_ms: Option<f32>,
}

impl HudGpuTimer {
    pub(super) fn new(device: &wgpu::Device, period_ns: f32) -> Self {
        let slots = std::array::from_fn(|_| HudGpuReadback {
            resolve: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spall-hud-query-resolve"),
                size: 16,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            readback: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spall-hud-query-readback"),
                size: 16,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            receiver: None,
        });
        Self {
            queries: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("spall-hud-timestamps"),
                ty: wgpu::QueryType::Timestamp,
                count: 2,
            }),
            slots,
            period_ns,
            latest_ms: None,
        }
    }

    pub(super) fn acquire(&mut self) -> (Option<usize>, Option<f32>) {
        let mut completed_ms = None;
        for slot in &mut self.slots {
            let Some(receiver) = slot.receiver.take() else {
                continue;
            };
            match receiver.try_recv() {
                Ok(Ok(())) => {
                    if let Ok(mapped) = slot.readback.slice(..).get_mapped_range() {
                        let ticks = bytemuck::cast_slice::<u8, u64>(&mapped);
                        if ticks.len() >= 2 {
                            let measured = (ticks[1].saturating_sub(ticks[0]) as f32
                                * self.period_ns
                                / 1_000_000.0,);
                            self.latest_ms = Some(measured.0);
                            completed_ms = Some(measured.0);
                        }
                    }
                    slot.readback.unmap();
                }
                Ok(Err(_)) => {}
                Err(mpsc::TryRecvError::Empty) => {
                    slot.receiver = Some(receiver);
                }
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        }
        let index = self.slots.iter().position(|slot| slot.receiver.is_none());
        (index, completed_ms)
    }

    pub(super) fn write_start(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.write_timestamp(&self.queries, 0);
    }

    pub(super) fn resolve(&self, encoder: &mut wgpu::CommandEncoder, slot: usize) {
        encoder.write_timestamp(&self.queries, 1);
        encoder.resolve_query_set(&self.queries, 0..2, &self.slots[slot].resolve, 0);
        encoder.copy_buffer_to_buffer(
            &self.slots[slot].resolve,
            0,
            &self.slots[slot].readback,
            0,
            16,
        );
    }

    pub(super) fn map_after_submit(&mut self, slot: usize) {
        let (sender, receiver) = mpsc::channel();
        self.slots[slot]
            .readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        self.slots[slot].receiver = Some(receiver);
    }
}
