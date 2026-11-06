//! OLE drag and drop for a pegged file.
//!
//! Dragging offers `CF_HDROP` (Explorer, chat apps) and `PNG`; the returned
//! drop effect tells the app whether the file was moved or copied.
//!
//! [`drag_file`] calls `DoDragDrop`, which runs a modal loop on the calling
//! (UI) thread and pumps messages, so the app must call it from a message
//! handler after OLE was initialised on that thread.

use std::cell::Cell;
use std::mem::{ManuallyDrop, size_of};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
    DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, GlobalFree,
    HGLOBAL, HWND, OLE_E_ADVISENOTSUPPORTED, POINT, S_OK,
};
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
    IEnumFORMATETC_Impl, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{
    GLOBAL_ALLOC_FLAGS, GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DoDragDrop, IDropSource,
    IDropSource_Impl,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MK_RBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::DROPFILES;
use windows::core::{BOOL, Error, HRESULT, Ref, implement, w};

use crate::types::ImageData;

/// `IDataObject::GetCanonicalFormatEtc` returns this success code when it
/// copied the format. The Windows headers name the value
/// `DATA_S_SAMEFORMATETC`; the OLE documentation calls it `DATA_S_FMTETC`.
const DATA_S_FMTETC: HRESULT = DATA_S_SAMEFORMATETC;

/// `GMEM_MOVEABLE | GMEM_ZEROINIT` (0x42): OLE takes ownership of the block and
/// `GlobalLock` hands us a writable pointer.
const HGLOBAL_FLAGS: GLOBAL_ALLOC_FLAGS = GLOBAL_ALLOC_FLAGS(GMEM_MOVEABLE.0 | GMEM_ZEROINIT.0);

/// Starts `DoDragDrop` and returns the resulting `DROPEFFECT` bits
/// (`DROPEFFECT_COPY`, `DROPEFFECT_MOVE`, or 0 when nothing accepted).
///
/// Blocks until the drag ends; only call from the UI thread, and only from a
/// message handler (OLE pumps messages internally). `hwnd` and `image` are kept
/// for the drag preview and the bitmap payload of a later phase; the OLE drag
/// loop itself needs neither.
pub fn drag_file(
    _hwnd: HWND,
    path: &Path,
    _image: &ImageData,
    png: Vec<u8>,
) -> Result<u32, String> {
    // The drag must name the file, not a path relative to our working
    // directory: Explorer resolves the drop in the target's context.
    let path =
        std::path::absolute(path).map_err(|error| format!("std::path::absolute: {error}"))?;

    // SAFETY: the name is a static, NUL-terminated UTF-16 string.
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    if png_format == 0 {
        return Err("RegisterClipboardFormatW(\"PNG\") returned 0".to_string());
    }
    let png_format = u16::try_from(png_format)
        .map_err(|_| format!("RegisterClipboardFormatW(\"PNG\") returned {png_format}, which does not fit a clipboard format"))?;

    // Owned copies: the object outlives every caller-side buffer during the
    // modal drag loop, and OLE may ask for the data more than once. `png` is
    // moved in, not copied: a screenshot payload is megabytes.
    let object: IDataObject = FileDataObject {
        path,
        png,
        png_format,
    }
    .into();
    let source: IDropSource = FileDropSource.into();
    let mut effect = DROPEFFECT(0);

    // SAFETY: OLE/COM was initialised as an STA on this thread by the caller,
    // both objects are alive for the whole call, and `effect` is a live local.
    // DoDragDrop only calls back into this thread while it pumps messages.
    let result = unsafe {
        DoDragDrop(
            &object,
            &source,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
            &mut effect,
        )
    };
    result
        .ok()
        .map_err(|error| format!("DoDragDrop: {error}"))?;
    Ok(effect.0)
}

/// The `IDataObject` offered by one drag: the file and its encoded PNG.
#[implement(IDataObject)]
struct FileDataObject {
    /// Absolute path of the file, served as `CF_HDROP`.
    path: PathBuf,
    /// PNG bytes, served under the registered `PNG` clipboard format.
    png: Vec<u8>,
    /// Id returned by `RegisterClipboardFormatW("PNG")`.
    png_format: u16,
}

/// The formats this data object serves.
#[derive(Clone, Copy)]
enum OfferedFormat {
    FileDrop,
    Png,
}

impl FileDataObject {
    /// Validates a caller's `FORMATETC` and says which format it selects.
    ///
    /// `GetData` and `QueryGetData` both call this, so they accept and reject
    /// exactly the same requests.
    fn offered(&self, format: &FORMATETC) -> Result<OfferedFormat, HRESULT> {
        if format.cfFormat != CF_HDROP.0 && format.cfFormat != self.png_format {
            return Err(DV_E_FORMATETC);
        }
        if format.lindex != -1 {
            return Err(DV_E_LINDEX);
        }
        if format.tymed & (TYMED_HGLOBAL.0 as u32) == 0 {
            return Err(DV_E_TYMED);
        }
        Ok(if format.cfFormat == CF_HDROP.0 {
            OfferedFormat::FileDrop
        } else {
            OfferedFormat::Png
        })
    }
}

impl FileDataObject {
    /// One `FORMATETC` describing a format this object serves.
    fn format(&self, cf_format: u16) -> FORMATETC {
        FORMATETC {
            cfFormat: cf_format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        }
    }
}

/// `IEnumFORMATETC` over the two offered formats. OLE calls it on the UI
/// thread, so a `Cell` is enough to track the position.
#[implement(IEnumFORMATETC)]
struct FormatEnumerator {
    formats: [FORMATETC; 2],
    position: Cell<usize>,
}

impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
    fn Next(
        &self,
        celt: u32,
        rgelt: *mut FORMATETC,
        pceltfetched: *mut u32,
    ) -> windows::core::HRESULT {
        if rgelt.is_null() {
            return E_INVALIDARG;
        }
        let mut copied = 0u32;
        for index in 0..celt as usize {
            let source = self.position.get() + index;
            if source >= self.formats.len() {
                break;
            }
            // SAFETY: OLE guarantees room for `celt` FORMATETC entries.
            unsafe {
                *rgelt.add(index) = self.formats[source];
            }
            copied += 1;
        }
        self.position.set(self.position.get() + copied as usize);
        if !pceltfetched.is_null() {
            // SAFETY: OLE passes a writable out-parameter when it wants the count.
            unsafe {
                *pceltfetched = copied;
            }
        }
        if copied == celt {
            S_OK
        } else {
            windows::Win32::Foundation::S_FALSE
        }
    }

    fn Skip(&self, celt: u32) -> windows::core::Result<()> {
        let next = (self.position.get() + celt as usize).min(self.formats.len());
        self.position.set(next);
        if next >= self.formats.len() {
            Err(Error::from_hresult(windows::Win32::Foundation::S_FALSE))
        } else {
            Ok(())
        }
    }

    fn Reset(&self) -> windows::core::Result<()> {
        self.position.set(0);
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumFORMATETC> {
        let copy = FormatEnumerator {
            formats: self.formats,
            position: Cell::new(self.position.get()),
        };
        Ok(copy.into())
    }
}
impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        // SAFETY: OLE passes a FORMATETC that stays valid for the call; the
        // reference is only read and never escapes.
        let format = match unsafe { pformatetcin.as_ref() } {
            Some(format) => format,
            None => return Err(Error::from_hresult(E_INVALIDARG)),
        };

        // A fresh block per request: the target owns it from now on and frees
        // it with ReleaseStgMedium.
        let handle = match self.offered(format) {
            Ok(OfferedFormat::FileDrop) => alloc_hglobal(&dropfiles_bytes(&self.path))?,
            Ok(OfferedFormat::Png) => alloc_hglobal(&self.png)?,
            Err(code) => return Err(Error::from_hresult(code)),
        };

        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: handle },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        // We only support TYMED_HGLOBAL, where GetData hands over the block.
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        // SAFETY: as in GetData; the pointer is borrowed for this call only.
        let format = match unsafe { pformatetc.as_ref() } {
            Some(format) => format,
            None => return E_INVALIDARG,
        };
        match self.offered(format) {
            Ok(_) => S_OK,
            Err(code) => code,
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        if pformatectin.is_null() || pformatetcout.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: both pointers were checked non-null and OLE keeps the
        // FORMATETC values alive for the duration of the call.
        unsafe {
            let input = *pformatectin;
            // The canonical form of a format has no target device.
            pformatetcout.write(FORMATETC {
                ptd: std::ptr::null_mut(),
                ..input
            });
        }
        DATA_S_FMTETC
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> windows::core::Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        // DATADIR_SET (2) is not offered: this object is a source only.
        if dwdirection != 1 {
            return Err(Error::from_hresult(E_NOTIMPL));
        }
        let enumerator = FormatEnumerator {
            formats: [self.format(self.png_format), self.format(CF_HDROP.0)],
            position: Cell::new(0),
        };
        Ok(enumerator.into())
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<'_, IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED))
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED))
    }
}

/// Tells OLE whether a drag is still running, ended with a drop, or was
/// cancelled.
#[implement(IDropSource)]
struct FileDropSource;

impl IDropSource_Impl for FileDropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() || (grfkeystate.0 & MK_RBUTTON.0) != 0 {
            DRAGDROP_S_CANCEL
        } else if (grfkeystate.0 & MK_LBUTTON.0) == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Builds the `CF_HDROP` payload for one file: a `DROPFILES` header followed by
/// the absolute path in UTF-16, its NUL terminator, and the list's extra NUL.
fn dropfiles_bytes(path: &Path) -> Vec<u8> {
    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        pt: POINT::default(),
        fNC: BOOL::from(false),
        fWide: BOOL::from(true),
    };
    let units: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();

    let mut bytes = Vec::with_capacity(size_of::<DROPFILES>() + units.len() * 2);
    // SAFETY: DROPFILES is a plain C struct with every field initialised; the
    // slice covers exactly the struct's own bytes and does not outlive it.
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            (&header as *const DROPFILES).cast::<u8>(),
            size_of::<DROPFILES>(),
        )
    };
    bytes.extend_from_slice(header_bytes);
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Copies `bytes` into a moveable `HGLOBAL` for a `STGMEDIUM`.
///
/// On success OLE (or the drop target) owns the block; on failure it is freed
/// here. The block is unlocked before it is handed over.
fn alloc_hglobal(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    // OLE requires a non-empty medium; an empty payload still gets one byte.
    let size = bytes.len().max(1);

    // SAFETY: GlobalAlloc only takes a size and flags and returns an owned
    // handle (or an error); no pointer is retained.
    let handle = unsafe { GlobalAlloc(HGLOBAL_FLAGS, size) }?;

    // SAFETY: the block is ours and not shared yet; locking it once makes the
    // bytes writable until the matching GlobalUnlock below.
    let destination = unsafe { GlobalLock(handle) };
    if destination.is_null() {
        // SAFETY: the allocation is ours and has not been handed to OLE.
        unsafe {
            let _ = GlobalFree(Some(handle));
        }
        return Err(Error::from_hresult(E_OUTOFMEMORY));
    }

    // SAFETY: `destination` is the base of a block of at least `bytes.len()`
    // bytes, and the source is a live slice, so the regions are disjoint.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast::<u8>(), bytes.len());
    }

    // SAFETY: balances the GlobalLock above. A FALSE return only means the
    // lock count reached zero, not that the copy failed.
    unsafe {
        let _ = GlobalUnlock(handle);
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    /// The bytes must match what `CF_HDROP` consumers expect: a 20-byte
    /// `DROPFILES` header, then the wide path, then two NULs.
    #[test]
    fn dropfiles_bytes_layout() {
        let path = Path::new(r"C:\shots\pegged.png");
        let bytes = dropfiles_bytes(path);
        let header = std::mem::size_of::<DROPFILES>();

        assert_eq!(header, 20);
        assert_eq!(
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize,
            header
        );
        assert_eq!(i32::from_le_bytes(bytes[12..16].try_into().unwrap()), 0); // fNC
        assert_eq!(i32::from_le_bytes(bytes[16..20].try_into().unwrap()), 1); // fWide

        let units: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
        let expected: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        assert_eq!(bytes.len(), header + expected.len());
        assert_eq!(&bytes[header..], expected.as_slice());
        assert_eq!(&bytes[bytes.len() - 4..], &[0u8, 0, 0, 0][..]);
    }
}
