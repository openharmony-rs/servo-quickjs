/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! QuickJS shim: the Intl subset over the OS's ICU (OpenHarmony and Linux; elsewhere QuickJS has
//! no Intl). The expected strings follow ICU 74 (CLDR 44.1), the ICU of OpenHarmony 7.0 and of
//! Ubuntu 24.04.

use std::ptr::{self, NonNull};

use mozjs::context::JSContext;
use mozjs::conversions::jsstr_to_string;
use mozjs::jsapi::OnNewGlobalHookOption;
use mozjs::jsval::UndefinedValue;
use mozjs::rooted;
use mozjs::rust::wrappers2::JS_NewGlobalObject;
use mozjs::rust::{
    evaluate_script, CompileOptionsWrapper, HandleObject, JSEngine, RealmOptions, Runtime,
    SIMPLE_GLOBAL_CLASS,
};

/// The string value of `expr`, or "throws " and the error's name (and message if `verbose`).
unsafe fn eval(cx: &mut JSContext, global: HandleObject, expr: &str, verbose: bool) -> String {
    let message = if verbose { " + ': ' + e.message" } else { "" };
    let source = format!("String((() => {{ try {{ return {expr}; }} catch (e) {{ return 'throws ' + e.name{message}; }} }})())");
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"qjs_intl.js".to_owned(), 1);
    evaluate_script(cx, global, &source, rval.handle_mut(), options)
        .unwrap_or_else(|_| panic!("evaluating {expr:?} failed"));
    jsstr_to_string(cx, NonNull::new(rval.get().to_string()).unwrap())
}

unsafe fn check(cx: &mut JSContext, global: HandleObject, cases: &[(&str, &str)]) {
    let mut failures = Vec::new();
    for (expr, expected) in cases {
        let actual = eval(cx, global, expr, false);
        if actual != *expected {
            let actual = eval(cx, global, expr, true);
            failures.push(format!(
                "{expr}\n    expected {expected:?}\n    got      {actual:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn qjs_intl() {
    // Apps inherit ICU's data from appspawn; a test binary run from hdc shell has to find it.
    #[cfg(target_env = "ohos")]
    if std::env::var_os("ICU_DATA").is_none() {
        std::env::set_var("ICU_DATA", "/system/usr/icu");
    }
    let engine = JSEngine::init().unwrap();
    let mut runtime = Runtime::new(engine.handle());
    let cx = runtime.cx();
    unsafe {
        rooted!(&in(cx) let global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        let mut realm = mozjs::realm::AutoRealm::new_from_handle(cx, global.handle());
        let cx = &mut *realm;
        #[cfg(not(qjs_intl))]
        check(cx, global.handle(), &[("typeof Intl", "undefined")]);
        #[cfg(qjs_intl)]
        {
            from_pages(cx, global.handle());
            numbers(cx, global.handle());
            dates(cx, global.handle());
            web_compat(cx, global.handle());
            collation_and_case(cx, global.handle());
            locales_and_objects(cx, global.handle());
        }
    }
}

/// The calls of real pages that motivated the subset.
#[cfg(qjs_intl)]
unsafe fn from_pages(cx: &mut JSContext, global: HandleObject) {
    check(
        cx,
        global,
        &[
            (
                r#"new Date(2026, 9, 7).toLocaleDateString("zh-CN")"#,
                "2026/10/7",
            ),
            (
                r#"new Date(2026, 9, 7).toLocaleDateString("en-US", {day: "2-digit", month: "short"})"#,
                "Oct 07",
            ),
            (r#"(12345.678).toLocaleString("en-US")"#, "12,345.678"),
            (
                r#"["张三", "李四", "王五"].sort((a, b) => a.localeCompare(b, "zh-CN")).join()"#,
                "李四,王五,张三",
            ),
            (
                r#"new Intl.NumberFormat("zh-CN", {style: "percent"}).format(0.25)"#,
                "25%",
            ),
        ],
    );
}

#[cfg(qjs_intl)]
unsafe fn numbers(cx: &mut JSContext, global: HandleObject) {
    check(
        cx,
        global,
        &[
            (r#"(12345.678).toLocaleString("de-DE")"#, "12.345,678"),
            (
                r#"(1234.5).toLocaleString("en-US", {style: "currency", currency: "USD"})"#,
                "$1,234.50",
            ),
            (
                r#"(1234.5).toLocaleString("zh-CN", {style: "currency", currency: "CNY"})"#,
                "¥1,234.50",
            ),
            (
                r#"(1234.5).toLocaleString("en-US", {style: "currency", currency: "JPY"})"#,
                "¥1,235",
            ),
            (
                r#"(1234.5).toLocaleString("en-US", {style: "currency", currency: "EUR", currencyDisplay: "code"})"#,
                "EUR\u{a0}1,234.50",
            ),
            (
                r#"(2).toLocaleString("en-US", {style: "currency", currency: "USD", currencyDisplay: "name"})"#,
                "2.00 US dollars",
            ),
            (
                r#"(1.5).toLocaleString("en-US", {minimumFractionDigits: 2})"#,
                "1.50",
            ),
            (
                r#"(2.5).toLocaleString("en-US", {maximumFractionDigits: 0})"#,
                "3",
            ),
            (
                r#"(1.005).toLocaleString("en-US", {maximumFractionDigits: 2})"#,
                "1.01",
            ),
            (
                r#"(5).toLocaleString("en-US", {minimumIntegerDigits: 3})"#,
                "005",
            ),
            (
                r#"(12345).toLocaleString("en-US", {useGrouping: false})"#,
                "12345",
            ),
            (
                r#"(1234.5678).toLocaleString("en-US", {maximumSignificantDigits: 3})"#,
                "1,230",
            ),
            (
                r#"(12345).toLocaleString("en-US", {notation: "compact"})"#,
                "12K",
            ),
            (
                r#"(12345).toLocaleString("zh-CN", {notation: "compact"})"#,
                "1.2万",
            ),
            (
                r#"(5).toLocaleString("en-US", {signDisplay: "always"})"#,
                "+5",
            ),
            (
                r#"(0).toLocaleString("en-US", {signDisplay: "exceptZero"})"#,
                "0",
            ),
            (r#"(-0).toLocaleString("en-US")"#, "-0"),
            (r#"NaN.toLocaleString("en-US")"#, "NaN"),
            (
                r#"new Intl.NumberFormat("en-US").format(12345678901234567890n)"#,
                "12,345,678,901,234,567,890",
            ),
            (
                r#"new Intl.NumberFormat("en-US").formatToParts(-12345.6).map(p => p.type + ":" + p.value).join("|")"#,
                "minusSign:-|integer:12|group:,|integer:345|decimal:.|fraction:6",
            ),
            (
                r#"JSON.stringify(new Intl.NumberFormat("en-US", {style: "currency", currency: "usd"}).resolvedOptions())"#,
                r#"{"locale":"en-US","numberingSystem":"latn","style":"currency","currency":"USD","currencyDisplay":"symbol","currencySign":"standard","minimumIntegerDigits":1,"minimumFractionDigits":2,"maximumFractionDigits":2,"useGrouping":"auto","notation":"standard","signDisplay":"auto","roundingIncrement":1,"roundingMode":"halfExpand","roundingPriority":"auto","trailingZeroDisplay":"auto"}"#,
            ),
            (
                r#"new Intl.NumberFormat("en", {style: "currency"})"#,
                "throws TypeError",
            ),
            (
                r#"new Intl.NumberFormat("en", {style: "unit", unit: "meter"})"#,
                "throws RangeError",
            ),
            (
                r#"new Intl.NumberFormat("en", {maximumFractionDigits: 101})"#,
                "throws RangeError",
            ),
            (
                r#"Number.prototype.toLocaleString.call("1")"#,
                "throws TypeError",
            ),
        ],
    );
}

#[cfg(qjs_intl)]
unsafe fn dates(cx: &mut JSContext, global: HandleObject) {
    let t = "Date.UTC(2026, 9, 7, 15, 5, 9, 25)";
    let utc = |expr: &str| expr.replace("$T", t);
    let cases: Vec<(String, &str)> = vec![
        (utc(r#"new Date($T).toLocaleString("en-US", {timeZone: "UTC"})"#), "10/7/2026, 3:05:09 PM"),
        (utc(r#"new Date($T).toLocaleString("zh-CN", {timeZone: "UTC"})"#), "2026/10/7 下午3:05:09"),
        (utc(r#"new Date($T).toLocaleTimeString("en-GB", {timeZone: "UTC"})"#), "15:05:09"),
        (utc(r#"new Date($T).toLocaleDateString("en-US", {dateStyle: "long", timeZone: "UTC"})"#), "October 7, 2026"),
        (utc(r#"new Date($T).toLocaleDateString("zh-CN", {dateStyle: "full", timeZone: "UTC"})"#), "2026年10月7日星期三"),
        (utc(r#"new Date($T).toLocaleString("en-US", {dateStyle: "short", timeStyle: "short", timeZone: "UTC"})"#), "10/7/26, 3:05 PM"),
        (utc(r#"new Date($T).toLocaleTimeString("en-US", {timeStyle: "short", hour12: false, timeZone: "UTC"})"#), "15:05"),
        (utc(r#"new Date($T).toLocaleDateString("en-US", {weekday: "long", year: "numeric", month: "long", day: "numeric", timeZone: "UTC"})"#), "Wednesday, October 7, 2026"),
        (utc(r#"new Date($T).toLocaleDateString("zh-CN", {month: "long", day: "numeric", timeZone: "UTC"})"#), "10月7日"),
        (utc(r#"new Date($T).toLocaleDateString("ja-JP", {year: "numeric", month: "short", day: "numeric", weekday: "short", timeZone: "UTC"})"#), "2026年10月7日(水)"),
        (utc(r#"new Date($T).toLocaleTimeString("en-US", {hour: "2-digit", minute: "2-digit", timeZone: "UTC"})"#), "03:05 PM"),
        (utc(r#"new Date($T).toLocaleTimeString("en-US", {hour: "numeric", minute: "2-digit", hour12: false, timeZone: "UTC"})"#), "15:05"),
        (utc(r#"new Date($T).toLocaleTimeString("zh-CN", {hour: "numeric", minute: "2-digit", hourCycle: "h23", timeZone: "UTC"})"#), "15:05"),
        (utc(r#"new Date($T).toLocaleTimeString("en-US", {hour: "numeric", minute: "2-digit", second: "2-digit", fractionalSecondDigits: 3, timeZone: "UTC"})"#), "3:05:09.025 PM"),
        (utc(r#"new Date($T).toLocaleTimeString("en-US", {timeZone: "Asia/Shanghai", timeZoneName: "short"})"#), "11:05:09 PM GMT+8"),
        (utc(r#"new Date($T).toLocaleDateString("de-AT", {timeZone: "UTC"})"#), "7.10.2026"),
        (utc(r#"new Intl.DateTimeFormat("en-US", {month: "short", day: "2-digit", timeZone: "UTC"}).formatToParts($T).map(p => p.type + ":" + p.value).join("|")"#), "month:Oct|literal: |day:07"),
        (utc(r#"JSON.stringify(new Intl.DateTimeFormat("en-US", {hour: "numeric", minute: "numeric", timeZone: "Asia/Shanghai"}).resolvedOptions())"#),
         r#"{"locale":"en-US","calendar":"gregory","numberingSystem":"latn","timeZone":"Asia/Shanghai","hourCycle":"h12","hour12":true,"hour":"numeric","minute":"2-digit"}"#),
        (r#"JSON.stringify(new Intl.DateTimeFormat("zh-CN", {timeZone: "utc"}).resolvedOptions())"#.into(),
         r#"{"locale":"zh-CN","calendar":"gregory","numberingSystem":"latn","timeZone":"UTC","year":"numeric","month":"numeric","day":"numeric"}"#),
        (r#"new Date(2026, 9, 7, 12, 30).toLocaleTimeString("en-US", {hour: "numeric", minute: "numeric", hour12: false})"#.into(), "12:30"),
        (r#"new Date(NaN).toLocaleString("en-US")"#.into(), "Invalid Date"),
        (r#"new Intl.DateTimeFormat("en-US").format(NaN)"#.into(), "throws RangeError"),
        (r#"new Date(0).toLocaleDateString("en-US", {timeStyle: "short"})"#.into(), "throws TypeError"),
        (r#"new Intl.DateTimeFormat("en-US", {dateStyle: "short", year: "numeric"})"#.into(), "throws TypeError"),
        (r#"new Intl.DateTimeFormat("en-US", {timeZone: "Mars/Base"})"#.into(), "throws RangeError"),
        (r#"Date.prototype.toLocaleString.call({})"#.into(), "throws TypeError"),
        (r#"typeof new Date().toLocaleString()"#.into(), "string"),
    ];
    let cases: Vec<(&str, &str)> = cases.iter().map(|(e, x)| (e.as_str(), *x)).collect();
    check(cx, global, &cases);
}

/// The web compatibility requirements of WPT's ecmascript/locale-compat.html.
#[cfg(qjs_intl)]
unsafe fn web_compat(cx: &mut JSContext, global: HandleObject) {
    check(
        cx,
        global,
        &[
            (
                r#"/:\d\d:\d\d\x20[AP]M$/.test(new Date("2022-12-31T23:59:42").toLocaleString("en-US"))"#,
                "true",
            ),
            (
                r#"/:\d\d:\d\d\x20[AP]M /.test(new Intl.DateTimeFormat("en", {timeStyle: "long"}).format(new Date("2022-12-31T23:59:42")))"#,
                "true",
            ),
            (
                r#"new Intl.DateTimeFormat("en-US", {hour: "numeric", minute: "2-digit", timeZone: "UTC"}).formatToParts(0).map(p => p.value).join("|")"#,
                "12|:|00| |AM",
            ),
            (
                r#"new Date("2022-03-31T23:59:42").toLocaleString("en-CA").startsWith("2022-03-31")"#,
                "true",
            ),
            (
                r#"new Date("2022-03-31T23:59:42").toLocaleString("en-CA", {dateStyle: "short"})"#,
                "2022-03-31",
            ),
            (
                r#"new Intl.DateTimeFormat("sv-SE").format(new Date("1970-01-01T00:00:00"))"#,
                "1970-01-01",
            ),
        ],
    );
}

#[cfg(qjs_intl)]
unsafe fn collation_and_case(cx: &mut JSContext, global: HandleObject) {
    check(
        cx,
        global,
        &[
            (
                r#"new Intl.Collator("en", {sensitivity: "base"}).compare("a", "A")"#,
                "0",
            ),
            (r#"new Intl.Collator("en").compare("a", "A")"#, "-1"),
            (
                r#"new Intl.Collator("en", {sensitivity: "accent"}).compare("a", "á")"#,
                "-1",
            ),
            (
                r#"["a10", "a2", "a1"].sort(new Intl.Collator("en", {numeric: true}).compare).join()"#,
                "a1,a2,a10",
            ),
            (
                r#"["a10", "a2"].sort(new Intl.Collator("en-u-kn").compare).join()"#,
                "a2,a10",
            ),
            (
                r#"["b", "a-c", "ab"].sort(new Intl.Collator("en", {ignorePunctuation: true}).compare).join()"#,
                "ab,a-c,b",
            ),
            (
                r#"["z", "ä", "a"].sort(new Intl.Collator("de").compare).join()"#,
                "a,ä,z",
            ),
            (
                r#"["z", "ä", "a"].sort(new Intl.Collator("sv").compare).join()"#,
                "a,z,ä",
            ),
            (r#""a".localeCompare("B")"#, "-1"),
            (
                r#"JSON.stringify(new Intl.Collator("de-u-co-phonebk").resolvedOptions())"#,
                r#"{"locale":"de-u-co-phonebk","usage":"sort","sensitivity":"variant","ignorePunctuation":false,"collation":"phonebk","numeric":false,"caseFirst":"false"}"#,
            ),
            (
                r#"new Intl.Collator("da").resolvedOptions().caseFirst"#,
                "upper",
            ),
            (
                r#"new Intl.Collator("da", {caseFirst: "false"}).resolvedOptions().caseFirst"#,
                "false",
            ),
            (
                r#"new Intl.Collator("th").resolvedOptions().ignorePunctuation"#,
                "true",
            ),
            (r#""i".toLocaleUpperCase("tr")"#, "İ"),
            (r#""I".toLocaleLowerCase("tr")"#, "ı"),
            (r#""i".toLocaleUpperCase("en")"#, "I"),
            (r#""straße".toLocaleUpperCase("de")"#, "STRASSE"),
            (
                r#"String.prototype.localeCompare.call(null, "a")"#,
                "throws TypeError",
            ),
        ],
    );
}

#[cfg(qjs_intl)]
unsafe fn locales_and_objects(cx: &mut JSContext, global: HandleObject) {
    check(
        cx,
        global,
        &[
            (r#"Object.prototype.toString.call(Intl)"#, "[object Intl]"),
            (
                r#"Intl.getCanonicalLocales(["EN-us", "zh-cn", "en-US"]).join()"#,
                "en-US,zh-CN",
            ),
            (r#"Intl.getCanonicalLocales("en_US")"#, "throws RangeError"),
            (
                r#"Intl.NumberFormat.supportedLocalesOf(["zh-CN", "xx-YY", "en-GB"]).join()"#,
                "zh-CN,en-GB",
            ),
            (
                r#"new Intl.NumberFormat("xx-YY").resolvedOptions().locale === new Intl.NumberFormat().resolvedOptions().locale"#,
                "true",
            ),
            (
                r#"new Intl.NumberFormat(["xx", "de-CH"]).resolvedOptions().locale"#,
                "de-CH",
            ),
            (r#"Intl.NumberFormat("en").format(1)"#, "1"),
            (
                r#"(() => { const f = new Intl.NumberFormat("en"); return f.format === f.format; })()"#,
                "true",
            ),
            (
                r#"(() => { const f = new Intl.NumberFormat("en"); return [1000, 2000].map(f.format).join("|"); })()"#,
                "1,000|2,000",
            ),
            (
                r#"(() => { class X extends Intl.NumberFormat {} const x = new X("en"); return x instanceof X && x.format(1234); })()"#,
                "1,234",
            ),
            (
                r#"Object.prototype.toString.call(new Intl.DateTimeFormat())"#,
                "[object Intl.DateTimeFormat]",
            ),
            (
                r#"Intl.NumberFormat.prototype.resolvedOptions.call(new Intl.Collator())"#,
                "throws TypeError",
            ),
            (
                r#"(() => { for (let i = 0; i < 2000; i++) new Intl.Collator("en", {numeric: i % 2 == 0}).compare; return "ok"; })()"#,
                "ok",
            ),
        ],
    );
}
