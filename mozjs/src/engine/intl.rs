/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! A minimal ECMA-402 subset over the OS's ICU, installed on every new realm.
//!
//! QuickJS has no `Intl` and ignores locales and options in `toLocaleString`, `localeCompare` and
//! the like. The OpenHarmony NDK exports ICU4C's C API (`libicu.so`), and so does the system ICU of
//! other Linux, so there this adds:
//! - `Intl.Collator` and `String.prototype.localeCompare` (`ucol`);
//! - `Intl.NumberFormat` and `Number.prototype.toLocaleString` (`unum`: the NDK exports
//!   `unumf_formatDouble` but not `unumf_openResult`, so number skeletons cannot be used);
//! - `Intl.DateTimeFormat` and `Date.prototype.toLocale{,Date,Time}String` (`udat`).
//!   `dateStyle`/`timeStyle` use ICU's styles. Component options (`{month: "short"}`) need a
//!   pattern generator, which the NDK lacks (`udatpg`): `intl/dtpg.rs` ports ICU's over the CLDR
//!   availableFormats of a fixed locale list (`intl/patterns.rs`: en-US, en-GB, zh-CN, zh-TW,
//!   zh-HK, zh-MO, zh-SG, zh-Hans-HK, zh-Hans-MO, ja, ko, de, fr, es, ru, en-CA, sv-SE; see
//!   `tools/gen_intl_patterns.py` to add one).
//!   Other locales get ICU's style patterns of the locale plus root's availableFormats. As in
//!   SpiderMonkey, formatted dates and times have spaces for CLDR's narrow and thin spaces.
//! - `String.prototype.toLocale{Upper,Lower}Case` (`u_strToUpper`/`u_strToLower`);
//! - `Intl.getCanonicalLocales` and `supportedLocalesOf`.
//!
//! A system ICU has the pattern generator and number skeletons, but Linux uses the same code as
//! OpenHarmony, so that a desktop build behaves like the device.
//!
//! The default time zone is the one `Date` uses: `TZ`, else `OH_TimeService_GetTimeZone` on
//! OpenHarmony, where ICU's default outside app processes is only a guess, and ICU's on Linux.
//! Apps get ICU's data from appspawn; standalone executables (tests run from hdc shell) need
//! `ICU_DATA=/system/usr/icu`.
//!
//! Elsewhere, or without the `intl` feature, nothing is installed and QuickJS's behaviour stays.
//! `build.rs` sets `qjs_intl` when the subset is built.

#[cfg(any(qjs_intl, test))]
#[cfg_attr(not(qjs_intl), allow(dead_code))]
mod dtpg;
#[cfg(qjs_intl)]
mod icu;
#[cfg(qjs_intl)]
mod js;
#[cfg(any(qjs_intl, test))]
#[cfg_attr(not(qjs_intl), allow(dead_code))]
mod patterns;

use servo_quickjs as q;

/// Installs the Intl subset in a new QuickJS context, before its global gets its class.
pub(crate) unsafe fn install_intl(ctx: *mut q::JSContext) {
    #[cfg(qjs_intl)]
    js::install(ctx);
    #[cfg(not(qjs_intl))]
    let _ = ctx;
}
