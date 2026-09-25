//! The output-size policy's GPU scale pass: draw a source region into a
//! private texture of the output size, preserving aspect (with black bars when
//! the output is fixed). The result is then copied into a pool slot like any
//! captured frame; both run in order on the device's immediate context.

use ::windows::{
    Win32::Graphics::{
        Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile, ID3DBlob},
        Direct3D11::{
            D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER,
            D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_SAMPLER_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            D3D11_VIEWPORT, ID3D11Buffer, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView,
            ID3D11Texture2D, ID3D11VertexShader,
        },
    },
    core::{Interface, PCSTR, s},
};

use crate::{
    error::Result,
    native::windows::{D3d11Device, failure, texture_desc},
};

const SHADER: &str = r"
cbuffer Source : register(b0) { float4 source; };
struct V { float4 position : SV_Position; float2 uv : TEXCOORD0; };
V vs(uint id : SV_VertexID) {
    float2 t = float2((id << 1) & 2, id & 2);
    V v;
    v.position = float4(t * float2(2, -2) + float2(-1, 1), 0, 1);
    v.uv = source.xy + t * source.zw;
    return v;
}
Texture2D image : register(t0);
SamplerState linear_clamp : register(s0);
float4 ps(V v) : SV_Target { return image.Sample(linear_clamp, v.uv); }
";

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    // SAFETY: the source is a live static string; out pointers are live locals.
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            s!("jackstay-scale"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(error) = result {
        let detail = errors.map(|blob| {
            // SAFETY: the error blob holds GetBufferSize bytes of text.
            let bytes = unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) };
            String::from_utf8_lossy(bytes).into_owned()
        });
        return Err(failure("scale-compile", format!("{error}: {}", detail.unwrap_or_default())));
    }
    code.ok_or_else(|| failure("scale-compile", "no bytecode"))
}

fn bytecode(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob owns GetBufferSize bytes for its lifetime.
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) }
}

/// Where a source region lands in the output: its viewport, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Placement {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

/// Cached shader views: two WGC buffers, their predecessors across one
/// resize, and a private source copy.
const VIEW_CACHE: usize = 5;

pub(super) struct Scaler {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    /// The source rectangle in UV space, updated in place per frame.
    constants: ID3D11Buffer,
    views: Vec<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
    target: Option<(u32, u32, ID3D11Texture2D, ID3D11RenderTargetView)>,
    /// A private copy of the source when the captured texture cannot be
    /// sampled directly.
    source: Option<(u32, u32, ID3D11Texture2D)>,
}

impl Scaler {
    pub(super) fn new(device: &D3d11Device) -> Result<Self> {
        let raw = device.raw();
        let vs = compile(s!("vs"), s!("vs_4_0"))?;
        let ps = compile(s!("ps"), s!("ps_4_0"))?;
        let mut vertex = None;
        let mut pixel = None;
        let mut sampler = None;
        let mut constants = None;
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let constants_desc = D3D11_BUFFER_DESC {
            ByteWidth: 16,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        // SAFETY: bytecode and descriptions are live; out pointers are locals.
        unsafe {
            raw.CreateVertexShader(bytecode(&vs), None, Some(&mut vertex))
                .map_err(|error| failure("scale-shader", error))?;
            raw.CreatePixelShader(bytecode(&ps), None, Some(&mut pixel))
                .map_err(|error| failure("scale-shader", error))?;
            raw.CreateSamplerState(&sampler_desc, Some(&mut sampler))
                .map_err(|error| failure("scale-sampler", error))?;
            raw.CreateBuffer(&constants_desc, None, Some(&mut constants))
                .map_err(|error| failure("scale-constants", error))?;
        }
        Ok(Self {
            vertex: vertex.ok_or_else(|| failure("scale-shader", "no vertex shader"))?,
            pixel: pixel.ok_or_else(|| failure("scale-shader", "no pixel shader"))?,
            sampler: sampler.ok_or_else(|| failure("scale-sampler", "no sampler"))?,
            constants: constants.ok_or_else(|| failure("scale-constants", "no buffer"))?,
            views: Vec::new(),
            target: None,
            source: None,
        })
    }

    /// A shader view of `texture`, cached by texture identity. WGC cycles a
    /// few buffers (two, recreated on resize); the cache holds each texture it
    /// names, so an identity is never reused while cached.
    fn view(&mut self, device: &D3d11Device, texture: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
        if let Some((_, view)) = self.views.iter().find(|(cached, _)| cached.as_raw() == texture.as_raw()) {
            return Ok(view.clone());
        }
        let mut view = None;
        // SAFETY: the texture belongs to this device; the out pointer is a local.
        unsafe { device.raw().CreateShaderResourceView(texture, None, Some(&mut view)) }.map_err(|error| failure("scale-view", error))?;
        let view = view.ok_or_else(|| failure("scale-view", "no view"))?;
        if self.views.len() >= VIEW_CACHE {
            self.views.remove(0);
        }
        self.views.push((texture.clone(), view.clone()));
        Ok(view)
    }

    /// Draw `region` (left, top, width, height) of `texture` at `placement`
    /// inside a `size` output, black elsewhere. Returns the output texture,
    /// valid until the next call; its contents are ordered on the context.
    pub(super) fn render(
        &mut self,
        device: &D3d11Device,
        texture: &ID3D11Texture2D,
        region: (u32, u32, u32, u32),
        size: (u32, u32),
        placement: Placement,
    ) -> Result<ID3D11Texture2D> {
        let raw = device.raw();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: GetDesc fills a local structure.
        unsafe { texture.GetDesc(&mut desc) };
        if self.target.as_ref().is_none_or(|(width, height, ..)| (*width, *height) != size) {
            let target_desc = texture_desc(size.0, size.1, desc.Format, false);
            let mut target = None;
            let mut view = None;
            // SAFETY: descriptions and out pointers are live locals.
            unsafe {
                raw.CreateTexture2D(&target_desc, None, Some(&mut target))
                    .map_err(|error| failure("scale-target", error))?;
                let target = target.as_ref().ok_or_else(|| failure("scale-target", "no texture"))?;
                raw.CreateRenderTargetView(target, None, Some(&mut view))
                    .map_err(|error| failure("scale-target", error))?;
            }
            self.target = Some((
                size.0,
                size.1,
                target.expect("created above"),
                view.ok_or_else(|| failure("scale-target", "no view"))?,
            ));
        }
        let context = device.context();
        // A directly sampleable capture texture needs no private copy.
        let (sampled, uv_origin, extent) = if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 != 0 {
            (texture.clone(), (region.0, region.1), (desc.Width, desc.Height))
        } else {
            if self
                .source
                .as_ref()
                .is_none_or(|(width, height, _)| (*width, *height) != (region.2, region.3))
            {
                let mut copy = None;
                // SAFETY: description and out pointer are live locals.
                unsafe { raw.CreateTexture2D(&texture_desc(region.2, region.3, desc.Format, false), None, Some(&mut copy)) }
                    .map_err(|error| failure("scale-source", error))?;
                self.source = Some((region.2, region.3, copy.ok_or_else(|| failure("scale-source", "no texture"))?));
            }
            let copy = &self.source.as_ref().expect("created above").2;
            let bounds = ::windows::Win32::Graphics::Direct3D11::D3D11_BOX {
                left: region.0,
                top: region.1,
                front: 0,
                right: region.0 + region.2,
                bottom: region.1 + region.3,
                back: 1,
            };
            // SAFETY: both textures belong to this device; the context is locked.
            unsafe { context.CopySubresourceRegion(copy, 0, 0, 0, 0, texture, 0, Some(&bounds)) };
            (copy.clone(), (0, 0), (region.2, region.3))
        };
        let uv = [
            uv_origin.0 as f32 / extent.0 as f32,
            uv_origin.1 as f32 / extent.1 as f32,
            region.2 as f32 / extent.0 as f32,
            region.3 as f32 / extent.1 as f32,
        ];
        drop(context);
        let view = self.view(device, &sampled)?;
        let context = device.context();
        // SAFETY: 16 bytes of source for the 16-byte constant buffer; the
        // context is locked.
        unsafe { context.UpdateSubresource(&self.constants, 0, None, uv.as_ptr().cast(), 0, 0) };
        let (_, _, output, target) = self.target.as_ref().expect("created above");
        let viewport = D3D11_VIEWPORT {
            TopLeftX: placement.left,
            TopLeftY: placement.top,
            Width: placement.width,
            Height: placement.height,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        // SAFETY: every bound object belongs to this device; the context is
        // locked; bindings are cleared afterwards so later copies see no
        // resource bound for both reading and writing.
        unsafe {
            context.ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0]);
            context.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex, None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetShader(&self.pixel, None);
            context.PSSetShaderResources(0, Some(&[Some(view)]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None);
        }
        Ok(output.clone())
    }
}
