//! OpenDocument Spreadsheet (`.ods`, ODF 1.3) import and export. All fixtures are authored in
//! Rust; behaviour was compared against LibreOffice's own rendering of sample files.
//! Format reference: OASIS ODF 1.3, parts 2 (OpenFormula) and 3 (schema):
//! <https://docs.oasis-open.org/office/OpenDocument/v1.3/os/part3-schema/OpenDocument-v1.3-os-part3-schema.html>.

mod b64;
mod chart;
mod dom;
mod formula;
mod names;
mod numfmt;
mod page;
mod pivot;
mod read;
mod settings;
mod shape;
mod style;
mod write;

pub(crate) use read::has_mimetype;
pub use read::{is_encrypted_ods, is_flat_ods, read_fods, read_ods};
pub use write::{Display as OdsDisplay, write_fods, write_ods, write_ods_with};
