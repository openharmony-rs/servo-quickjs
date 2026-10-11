/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The original builtins of each realm, taken when the realm is created, before any of its
//! scripts run. The shim's own operations call these, never properties that scripts can replace,
//! as SpiderMonkey runs its builtins directly. They are kept by the realm's key object
//! (`GetRealmKeyObject`), which the global holds, together with the cross-origin property holders
//! cached for the realm.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use servo_quickjs as q;

use super::class::object_data;
use super::Cx;

pub(crate) const CLASS_OBJECT: q::JSClassID = 1;
pub(crate) const CLASS_ARRAY: q::JSClassID = 2;
pub(crate) const CLASS_SYMBOL: q::JSClassID = 7;
pub(crate) const CLASS_DATE: q::JSClassID = 10;
pub(crate) const CLASS_REGEXP: q::JSClassID = 18;
pub(crate) const CLASS_MAP: q::JSClassID = 35;
pub(crate) const CLASS_SET: q::JSClassID = 36;
pub(crate) const CLASS_WEAKMAP: q::JSClassID = 37;

/// Where an intrinsic is read from in a new realm.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Intrinsic {
    /// `%Class.prototype%[name]`.
    Method(q::JSClassID, &'static CStr),
    /// The getter of `%Class.prototype%[name]`.
    Getter(q::JSClassID, &'static CStr),
    /// `globalThis[name]`, or `globalThis[name][member]`.
    Global(&'static CStr, Option<&'static CStr>),
}

use Intrinsic::*;

const INTRINSICS: &[Intrinsic] = &[
    Method(CLASS_MAP, c"constructor"),
    Method(CLASS_MAP, c"get"),
    Method(CLASS_MAP, c"has"),
    Method(CLASS_MAP, c"set"),
    Method(CLASS_MAP, c"delete"),
    Method(CLASS_MAP, c"clear"),
    Method(CLASS_MAP, c"keys"),
    Method(CLASS_MAP, c"values"),
    Method(CLASS_MAP, c"entries"),
    Method(CLASS_MAP, c"forEach"),
    Getter(CLASS_MAP, c"size"),
    Method(CLASS_SET, c"constructor"),
    Method(CLASS_SET, c"has"),
    Method(CLASS_SET, c"add"),
    Method(CLASS_SET, c"delete"),
    Method(CLASS_SET, c"clear"),
    Method(CLASS_SET, c"keys"),
    Method(CLASS_SET, c"values"),
    Method(CLASS_SET, c"entries"),
    Method(CLASS_SET, c"forEach"),
    Getter(CLASS_SET, c"size"),
    Method(CLASS_WEAKMAP, c"get"),
    Method(CLASS_WEAKMAP, c"set"),
    Method(CLASS_DATE, c"getTime"),
    Method(CLASS_REGEXP, c"constructor"),
    Method(CLASS_REGEXP, c"exec"),
    Method(CLASS_ARRAY, c"values"),
    Method(CLASS_ARRAY, c"keys"),
    Method(CLASS_ARRAY, c"entries"),
    Method(CLASS_ARRAY, c"forEach"),
    Method(CLASS_OBJECT, c"constructor"),
    Global(c"JSON", Some(c"parse")),
    Getter(CLASS_SYMBOL, c"description"),
    Global(c"Symbol", Some(c"keyFor")),
];

/// Opaque data of a realm's key object.
struct Internals {
    /// Counted, in the order of `INTRINSICS`.
    intrinsics: Box<[q::JSValue]>,
    /// Counted: the cross-origin property holders of the realm, by the id of the holder map
    /// (`NewWeakMapObject`) of the object they were made for.
    holders: HashMap<u64, q::JSValue>,
}

static INTERNALS_CLASS: OnceLock<q::JSClassID> = OnceLock::new();
static HOLDER_MAP_CLASS: OnceLock<q::JSClassID> = OnceLock::new();

/// Holder map ids are never reused, so a freed map's entries can't be found through a new one.
static NEXT_HOLDER_MAP_ID: AtomicU64 = AtomicU64::new(1);

/// Which key objects hold an entry for which holder maps, so that the entries of freed maps are
/// dropped (`purge`) and the bookkeeping stays bounded.
#[derive(Default)]
struct HolderMaps {
    /// The number of key objects with an entry, by holder map id.
    entries: HashMap<u64, usize>,
    /// The freed holder maps that still have entries.
    freed: HashSet<u64>,
}

thread_local!(static HOLDER_MAPS: RefCell<HolderMaps> = RefCell::new(HolderMaps::default()));

/// Counts an entry less for holder map `id`.
fn forget_entry(maps: &mut HolderMaps, id: u64) {
    if let Some(n) = maps.entries.get_mut(&id) {
        *n -= 1;
        if *n == 0 {
            maps.entries.remove(&id);
            maps.freed.remove(&id);
        }
    }
}

/// Drops the entries of freed holder maps from a key object.
unsafe fn purge(rt: *mut q::JSRuntime, internals: &mut Internals) {
    let freed: Vec<u64> = HOLDER_MAPS.with(|m| {
        let m = m.borrow();
        if m.freed.is_empty() {
            return Vec::new();
        }
        internals
            .holders
            .keys()
            .copied()
            .filter(|id| m.freed.contains(id))
            .collect()
    });
    for id in freed {
        if let Some(holder) = internals.holders.remove(&id) {
            HOLDER_MAPS.with(|m| forget_entry(&mut m.borrow_mut(), id));
            q::JS_FreeValueRT(rt, holder);
        }
    }
}

unsafe fn class(
    rt: *mut q::JSRuntime,
    cell: &OnceLock<q::JSClassID>,
    def: q::JSClassDef,
) -> q::JSClassID {
    let id = *cell.get_or_init(|| {
        let mut id = 0;
        q::JS_NewClassID(&mut id);
        id
    });
    if q::JS_IsRegisteredClass(rt, id) == 0 {
        assert_eq!(q::JS_NewClass(rt, id, &def), 0);
    }
    id
}

unsafe fn internals_class(rt: *mut q::JSRuntime) -> q::JSClassID {
    class(
        rt,
        &INTERNALS_CLASS,
        q::JSClassDef {
            class_name: c"RealmKey".as_ptr(),
            finalizer: Some(internals_finalize),
            gc_mark: Some(internals_gc_mark),
            call: None,
            exotic: ptr::null_mut(),
        },
    )
}

unsafe fn holder_map_class(rt: *mut q::JSRuntime) -> q::JSClassID {
    class(
        rt,
        &HOLDER_MAP_CLASS,
        q::JSClassDef {
            class_name: c"CrossOriginPropertyHolderMap".as_ptr(),
            finalizer: Some(holder_map_finalize),
            gc_mark: None,
            call: None,
            exotic: ptr::null_mut(),
        },
    )
}

unsafe fn internals_of<'a>(v: q::JSValue) -> Option<&'a mut Internals> {
    let id = *INTERNALS_CLASS.get()?;
    (q::JS_GetOpaque(v, id) as *mut Internals).as_mut()
}

unsafe extern "C" fn internals_finalize(rt: *mut q::JSRuntime, val: q::JSValue) {
    let data = q::JS_GetOpaque(val, *INTERNALS_CLASS.get().unwrap()) as *mut Internals;
    if !data.is_null() {
        let data = Box::from_raw(data);
        let _ = HOLDER_MAPS.try_with(|m| {
            let mut m = m.borrow_mut();
            for id in data.holders.keys() {
                forget_entry(&mut m, *id);
            }
        });
        for v in data.intrinsics.iter().chain(data.holders.values()) {
            q::JS_FreeValueRT(rt, *v);
        }
    }
}

unsafe extern "C" fn holder_map_finalize(_rt: *mut q::JSRuntime, val: q::JSValue) {
    if let Some(id) = holder_map_id(val) {
        let _ = HOLDER_MAPS.try_with(|m| {
            let mut m = m.borrow_mut();
            if m.entries.contains_key(&id) {
                m.freed.insert(id);
            }
        });
    }
}

unsafe extern "C" fn internals_gc_mark(
    rt: *mut q::JSRuntime,
    val: q::JSValue,
    mark: q::JS_MarkFunc,
) {
    if let Some(data) = internals_of(val) {
        for v in data.intrinsics.iter().chain(data.holders.values()) {
            if v.has_ref_count() {
                q::JS_MarkValue(rt, *v, mark);
            }
        }
    }
}

/// Reads an intrinsic of `qctx` (a new reference, `JS_UNDEFINED` if it is missing).
unsafe fn read(qctx: *mut q::JSContext, which: Intrinsic) -> q::JSValue {
    let rt = q::JS_GetRuntime(qctx);
    match which {
        Method(class, name) => {
            let proto = q::JS_GetClassProto(qctx, class);
            let v = q::JS_GetPropertyStr(qctx, proto, name.as_ptr());
            q::JS_FreeValueRT(rt, proto);
            v
        }
        Getter(class, name) => {
            let proto = q::JS_GetClassProto(qctx, class);
            let atom = q::JS_NewAtom(qctx, name.as_ptr());
            let mut desc: q::JSPropertyDescriptor = std::mem::zeroed();
            let found = q::JS_GetOwnProperty(qctx, &mut desc, proto, atom);
            q::JS_FreeAtom(qctx, atom);
            q::JS_FreeValueRT(rt, proto);
            if found == 1 {
                q::JS_FreeValueRT(rt, desc.value);
                q::JS_FreeValueRT(rt, desc.setter);
                desc.getter
            } else {
                q::JS_UNDEFINED
            }
        }
        Global(name, member) => {
            let global = q::JS_GetGlobalObject(qctx);
            let v = q::JS_GetPropertyStr(qctx, global, name.as_ptr());
            q::JS_FreeValueRT(rt, global);
            match member {
                Some(member) => {
                    let m = q::JS_GetPropertyStr(qctx, v, member.as_ptr());
                    q::JS_FreeValueRT(rt, v);
                    m
                }
                None => v,
            }
        }
    }
}

/// A new key object for `qctx` with its intrinsics (a new reference). Must run before any
/// script of the realm does.
pub(crate) unsafe fn new_key_object(qctx: *mut q::JSContext) -> q::JSValue {
    let rt = q::JS_GetRuntime(qctx);
    let intrinsics = INTRINSICS.iter().map(|&which| read(qctx, which)).collect();
    let obj = q::JS_NewObjectProtoClass(qctx, q::JS_NULL, internals_class(rt));
    let data = Box::new(Internals {
        intrinsics,
        holders: HashMap::new(),
    });
    q::JS_SetOpaque(obj, Box::into_raw(data) as *mut c_void);
    obj
}

/// The key object of the current realm, or the runtime's outside any realm (not counted).
pub(crate) unsafe fn key_object(cx: &Cx) -> q::JSValue {
    let realm = cx.current_realm();
    if !realm.is_null() {
        if let Some(data) = object_data((*realm).global) {
            return *data.slots.last().unwrap();
        }
    }
    cx.base_key_object.get()
}

/// An intrinsic of the current realm (a new reference).
pub(crate) unsafe fn get(cx: &Cx, which: Intrinsic) -> q::JSValue {
    let index = INTRINSICS
        .iter()
        .position(|&i| i == which)
        .unwrap_or_else(|| panic!("intrinsic {which:?} is not taken"));
    let internals = internals_of(key_object(cx)).expect("realm without a key object");
    q::JS_DupValueRT(cx.rt, internals.intrinsics[index])
}

/// A new holder map (`NewWeakMapObject`).
pub(crate) unsafe fn new_holder_map(cx: &Cx) -> q::JSValue {
    let obj = q::JS_NewObjectProtoClass(cx.qctx(), q::JS_NULL, holder_map_class(cx.rt));
    let id = NEXT_HOLDER_MAP_ID.fetch_add(1, Ordering::Relaxed);
    q::JS_SetOpaque(obj, id as usize as *mut c_void);
    obj
}

fn holder_map_id(map: q::JSValue) -> Option<u64> {
    let class = *HOLDER_MAP_CLASS.get()?;
    let id = unsafe { q::JS_GetOpaque(map, class) } as usize as u64;
    (id != 0).then_some(id)
}

pub(crate) fn is_holder_map(map: q::JSValue) -> bool {
    holder_map_id(map).is_some()
}

/// The holder stored for `key`'s realm under `map` (a new reference, `JS_UNDEFINED` if none), or
/// `None` if `map` and `key` are not a holder map and a realm's key object.
pub(crate) unsafe fn holder(
    rt: *mut q::JSRuntime,
    map: q::JSValue,
    key: q::JSValue,
) -> Option<q::JSValue> {
    let id = holder_map_id(map)?;
    let internals = internals_of(key)?;
    purge(rt, internals);
    Some(match internals.holders.get(&id) {
        Some(&v) => q::JS_DupValueRT(rt, v),
        None => q::JS_UNDEFINED,
    })
}

/// Stores `value` for `key`'s realm under `map`; false if `map` and `key` are not a holder map and
/// a realm's key object.
pub(crate) unsafe fn set_holder(
    rt: *mut q::JSRuntime,
    map: q::JSValue,
    key: q::JSValue,
    value: q::JSValue,
) -> bool {
    let (Some(id), Some(internals)) = (holder_map_id(map), internals_of(key)) else {
        return false;
    };
    purge(rt, internals);
    match internals.holders.insert(id, q::JS_DupValueRT(rt, value)) {
        Some(old) => q::JS_FreeValueRT(rt, old),
        None => HOLDER_MAPS.with(|m| *m.borrow_mut().entries.entry(id).or_default() += 1),
    }
    true
}
