use std::fmt::{self, Debug, Formatter, Write};
use std::path::PathBuf;

use typst::diag::{format_xml_like_error, FileError};
use typst::eval::Datetime;
use typst::util::{hash128, AccessMode};

use crate::prelude::*;

/// Write plain text to a file.
///
/// The text will be added to a buffer and written once compilation is over.
/// Please note that this function does not ensure the call's order. Instead, you should make sure to add identifiers to your calls, if you want to find them later.
/// The file you write to will be named "record.txt", found in the same directory as your generated PDF/PNG(s).
/// We require a location to reduce de amount of code that depends on the
///
/// ## Example { #example }
/// ```example
/// #let text = write("data.html")
///
/// An example for a HTML file:\
/// #raw(text, lang: "html")
/// ```
///
/// Note to self: Could use macro Locatable instead
///
/// Display: Write
/// Category: data-loading
/*#[func]
pub fn write(
    /// The text to write.
    text: Spanned<EcoString>,
    /// The location one is writing from
    location: Location,
    /// The virtual machine.
    vm: &mut Vm,
) -> SourceResult<()> {
    let Spanned { v: text, span } = text;
    let path = "/record.txt";
    let path = vm.locate(path, AccessMode::W).at(span)?;
    vm.world().write(&path, hash128(&location), text.as_bytes().to_vec()).at(span)?;
    Ok(())
}*/


/// File descriptor used for convenience
#[derive(Clone, PartialEq, Hash)]
pub struct File(Str);

impl File {
    pub fn new(key: Str) -> Self {
        Self(key)
    }
}

impl Debug for File {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.write_str("file(")?;
        self.0.fmt(f)?;
        f.write_char(')')
    }
}

cast! {
    type File: "file",
}

/// Display: File
/// Category: data
#[func]
pub fn open(
    file: Str,
) -> File {
    File::new(file)
}
