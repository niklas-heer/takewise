//! The standard macOS About panel (Takewise › About Takewise): the name,
//! the version and a few lines of credits with a link. The icon and the
//! copyright come from the app bundle's `Info.plist`, as for any Mac app.

use objc2::runtime::AnyObject;
use objc2::{AnyThread as _, MainThreadMarker};
use objc2_app_kit::{
    NSAboutPanelOptionApplicationName, NSAboutPanelOptionApplicationVersion,
    NSAboutPanelOptionCredits, NSApplication, NSFont, NSFontAttributeName, NSLinkAttributeName,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSTextAlignment,
};
use objc2_foundation::{NSDictionary, NSMutableAttributedString, NSRange, NSString, NSURL};

use crate::error::{Error, Result};

/// What the About panel says.
#[derive(Debug, Clone, Copy)]
pub struct About<'a> {
    pub name: &'a str,
    /// The marketing version, such as "0.2.1"; the panel adds the bundle's
    /// build number.
    pub version: &'a str,
    /// One line on what the app is.
    pub description: &'a str,
    /// A link shown below the description: its text and where it goes.
    pub link_text: &'a str,
    pub link_url: &'a str,
}

/// Brings up the standard About panel. Main thread only.
pub fn show_about_panel(about: &About<'_>) -> Result<()> {
    let mtm = MainThreadMarker::new().ok_or(Error::NotMainThread)?;
    let url = NSURL::URLWithString(&NSString::from_str(about.link_url))
        .ok_or_else(|| Error::NotALink(about.link_url.to_owned()))?;
    let text = format!("{}\n\n{}", about.description, about.link_text);
    let utf16 = |s: &str| s.encode_utf16().count();
    let all = NSRange::new(0, utf16(&text));
    let link = NSRange::new(
        utf16(&text) - utf16(about.link_text),
        utf16(about.link_text),
    );
    let credits = NSMutableAttributedString::initWithString(
        NSMutableAttributedString::alloc(),
        &NSString::from_str(&text),
    );
    let font = NSFont::systemFontOfSize(NSFont::smallSystemFontSize());
    let paragraph = NSMutableParagraphStyle::new();
    paragraph.setAlignment(NSTextAlignment::Center);
    // SAFETY: each value has the type its attribute requires (an NSFont, an
    // NSParagraphStyle, an NSURL), the ranges lie within the string (counted
    // in UTF-16 units, as NSString does), and the keys are AppKit's
    // immutable constants.
    unsafe {
        credits.addAttribute_value_range(NSFontAttributeName, &font, all);
        credits.addAttribute_value_range(NSParagraphStyleAttributeName, &paragraph, all);
        credits.addAttribute_value_range(NSLinkAttributeName, &url, link);
    }
    let name = NSString::from_str(about.name);
    let version = NSString::from_str(about.version);
    // SAFETY: reading AppKit's immutable key constants.
    let keys = unsafe {
        [
            NSAboutPanelOptionApplicationName,
            NSAboutPanelOptionApplicationVersion,
            NSAboutPanelOptionCredits,
        ]
    };
    let objects: [&AnyObject; 3] = [&name, &version, &credits];
    let options = NSDictionary::from_slices(&keys, &objects);
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: on the main thread, with each option of the type its key
    // documents: the name and version NSStrings, the credits an
    // NSAttributedString.
    unsafe { app.orderFrontStandardAboutPanelWithOptions(&options) };
    Ok(())
}
