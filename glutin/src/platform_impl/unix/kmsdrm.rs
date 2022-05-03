#![cfg(feature = "kmsdrm")]

use drm::control::Device;
use gbm::{AsRaw, BufferObjectFlags};
use parking_lot::Mutex;
use winit::{
    event_loop::EventLoopWindowTarget,
    platform::unix::{AssertSync, EventLoopWindowTargetExtUnix},
    window::{Window, WindowBuilder},
};

use crate::{
    api::egl::NativeDisplay, ContextError, CreationError, GlAttributes, PixelFormat,
    PixelFormatRequirements, Rect,
};
use glutin_egl_sys as ffi;

use crate::api::egl::Context as EglContext;
use crate::api::egl::SurfaceType as EglSurfaceType;

macro_rules! pf_to_fmt {
    ($pf:expr) => {
        match ($pf.color_bits, $pf.alpha_bits) {
            (Some(24), Some(0) | None) => gbm::Format::Rgb888,
            (Some(16), Some(0) | None) => gbm::Format::Rgb565,
            (Some(8), Some(0) | None) => gbm::Format::Rgb332,
            (Some(15), Some(1)) => gbm::Format::Xrgb1555,
            (Some(30), Some(2)) => gbm::Format::Xrgb2101010,
            (Some(24), Some(8)) => gbm::Format::Xrgb8888,
            (Some(12), Some(4)) => gbm::Format::Xrgb4444,
            _ => gbm::Format::Xrgb8888,
        }
    };
}

#[derive(Debug)]
pub struct CtxLock {
    surface: Option<gbm::Surface<()>>,
    previous_bo: Option<gbm::BufferObject<()>>,
    previous_fb: Option<drm::control::framebuffer::Handle>,
}

#[derive(Debug)]
pub struct Context {
    display: EglContext,
    ctx_lock: parking_lot::Mutex<CtxLock>,
    device: AssertSync<gbm::Device<crate::platform::unix::Card>>,
    depth: u32,
    bpp: u32,
    connector: drm::control::connector::Handle,
    crtc: drm::control::crtc::Info,
    mode: drm::control::Mode,
}

impl std::ops::Deref for Context {
    type Target = EglContext;

    fn deref(&self) -> &Self::Target {
        &self.display
    }
}

impl Context {
    #[inline]
    pub fn new_headless<T>(
        el: &EventLoopWindowTarget<T>,
        pf_reqs: &PixelFormatRequirements,
        gl_attr: &GlAttributes<&Context>,
        _size: Option<winit::dpi::PhysicalSize<u32>>,
    ) -> Result<Self, CreationError> {
        let mut gl_attr = gl_attr.clone().map_sharing(|c| &**c);
        gl_attr.vsync = true;
        let drm_ptr = el
            .drm_device()
            .ok_or(CreationError::NotSupported("GBM is not initialized".into()))?
            .as_ref()
            .map_err(|e| CreationError::OsError(e.to_string()))?
            .clone();
        let display_ptr =
            gbm::Device::new(drm_ptr).map_err(|e| CreationError::OsError(e.to_string()))?;
        let native_display = NativeDisplay::Gbm(Some(display_ptr.as_raw() as *const _));
        let context = EglContext::new(
            pf_reqs,
            &gl_attr,
            native_display,
            EglSurfaceType::Surfaceless,
            |c, _| Ok(c[0]),
        )
        .and_then(|p| p.finish_surfaceless())?;
        let context = Context {
            display: context,
            ctx_lock: Mutex::new(CtxLock { surface: None, previous_fb: None, previous_bo: None }),
            device: AssertSync(display_ptr),
            depth: pf_reqs.depth_bits.unwrap_or(0) as u32,
            mode: el
                .drm_mode()
                .ok_or(CreationError::NotSupported("GBM is not initialized".into()))?,
            bpp: pf_reqs.alpha_bits.unwrap_or(0) as u32 + pf_reqs.color_bits.unwrap_or(0) as u32,
            crtc: el
                .drm_crtc()
                .ok_or(CreationError::NotSupported("GBM is not initialized".into()))?
                .clone(),
            connector: el
                .drm_connector()
                .ok_or(CreationError::NotSupported("GBM is not initialized".into()))?
                .handle(),
        };
        Ok(context)
    }

    #[inline]
    pub fn new<T>(
        wb: WindowBuilder,
        el: &EventLoopWindowTarget<T>,
        pf_reqs: &PixelFormatRequirements,
        gl_attr: &GlAttributes<&Context>,
    ) -> Result<(Window, Self), CreationError> {
        let window = wb.build(&el)?;
        let size = window.inner_size();
        let (width, height): (u32, u32) = size.into();
        let ctx = Self::new_raw_context(
            el.drm_device().ok_or(CreationError::NotSupported("GBM is not initialized".into()))?,
            width,
            height,
            el.drm_crtc().ok_or(CreationError::OsError("No crtc found".to_string()))?,
            el.drm_connector().ok_or(CreationError::OsError("No connector found".to_string()))?,
            el.drm_mode().ok_or(CreationError::OsError("No mode found".to_string()))?,
            pf_reqs,
            gl_attr,
        )?;
        Ok((window, ctx))
    }

    #[inline]
    pub fn new_raw_context(
        display_ptr: &'static AssertSync<Result<crate::platform::unix::Card, std::io::Error>>,
        width: u32,
        height: u32,
        crt: &drm::control::crtc::Info,
        con: &drm::control::connector::Info,
        mode: drm::control::Mode,
        pf_reqs: &PixelFormatRequirements,
        gl_attr: &GlAttributes<&Context>,
    ) -> Result<Self, CreationError> {
        let mut gl_attr = gl_attr.clone().map_sharing(|c| &**c);
        gl_attr.vsync = true;
        let drm_ptr =
            display_ptr.as_ref().map_err(|e| CreationError::OsError(e.to_string()))?.clone();
        let display_ptr =
            gbm::Device::new(drm_ptr).map_err(|e| CreationError::OsError(e.to_string()))?;
        let format = pf_to_fmt!(pf_reqs);

        let context = EglContext::new(
            pf_reqs,
            &gl_attr,
            NativeDisplay::Gbm(Some(display_ptr.as_raw() as ffi::EGLNativeDisplayType)),
            EglSurfaceType::Window,
            |c, _| Ok(c[0]),
        )?;

        let surface: gbm::Surface<()> = display_ptr
            .create_surface(
                width,
                height,
                format,
                BufferObjectFlags::SCANOUT | BufferObjectFlags::RENDERING,
            )
            .map_err(|e| CreationError::OsError(e.to_string()))?;

        let display = context.finish(surface.as_raw() as ffi::EGLNativeWindowType)?;

        let ctx = Context {
            display,
            mode,
            ctx_lock: Mutex::new(CtxLock {
                surface: Some(surface),
                previous_fb: None,
                previous_bo: None,
            }),
            device: AssertSync(display_ptr),
            depth: pf_reqs.depth_bits.unwrap_or(0) as u32,
            bpp: pf_reqs.alpha_bits.unwrap_or(0) as u32 + pf_reqs.color_bits.unwrap_or(0) as u32,
            crtc: crt.clone(),
            connector: con.handle(),
        };
        Ok(ctx)
    }

    #[inline]
    pub unsafe fn make_not_current(&self) -> Result<(), ContextError> {
        (**self).make_not_current()
    }

    #[inline]
    pub fn is_current(&self) -> bool {
        (**self).is_current()
    }

    #[inline]
    pub fn get_api(&self) -> crate::Api {
        (**self).get_api()
    }

    #[inline]
    pub unsafe fn raw_handle(&self) -> ffi::EGLContext {
        (**self).raw_handle()
    }

    #[inline]
    pub unsafe fn get_egl_display(&self) -> Option<*const std::os::raw::c_void> {
        Some((**self).get_egl_display())
    }

    #[inline]
    pub fn resize(&self, _width: u32, _height: u32) {
        /*
        match self {
        Context::Windowed(_, surface) => surface.0.resize(width as i32, height as i32, 0, 0),
        _ => unreachable!(),
        }
        */
    }

    #[inline]
    pub fn get_proc_address(&self, addr: &str) -> *const core::ffi::c_void {
        (**self).get_proc_address(addr)
    }

    #[inline]
    fn finish_swap_buffers(&self) -> Result<(), ContextError> {
        let mut lock = self.ctx_lock.lock();
        let front_buffer = unsafe {
            lock.surface
                .as_ref()
                .ok_or(ContextError::OsError("This context is surfaceless".to_string()))?
                .lock_front_buffer()
                .or_else(|e| {
                    Err(ContextError::OsError(format!("Error locking front buffer: {}", e)))
                })?
        };
        let fb = self
            .device
            .add_framebuffer(&front_buffer, self.depth, self.bpp)
            .or_else(|e| Err(ContextError::OsError(format!("Error adding framebuffer: {}", e))))?;
        self.device
            .set_crtc(self.crtc.handle(), Some(fb), (0, 0), &[self.connector], Some(self.mode))
            .or_else(|e| Err(ContextError::OsError(format!("Error setting crtc: {}", e))))?;
        if let Some(prev_fb) = lock.previous_fb {
            self.device.destroy_framebuffer(prev_fb).or_else(|e| {
                Err(ContextError::OsError(format!("Error destroying framebuffer: {}", e)))
            })?
        }
        lock.previous_fb = Some(fb);
        lock.previous_bo = Some(front_buffer);
        Ok(())
    }

    #[inline]
    pub fn swap_buffers(&self) -> Result<(), ContextError> {
        (**self).swap_buffers()?;
        self.finish_swap_buffers()
    }

    #[inline]
    pub fn swap_buffers_with_damage(&self, rects: &[Rect]) -> Result<(), ContextError> {
        (**self).swap_buffers_with_damage(rects)?;
        self.finish_swap_buffers()
    }

    #[inline]
    pub fn swap_buffers_with_damage_supported(&self) -> bool {
        (**self).swap_buffers_with_damage_supported()
    }

    #[inline]
    pub fn get_pixel_format(&self) -> PixelFormat {
        (**self).get_pixel_format().clone()
    }
}
