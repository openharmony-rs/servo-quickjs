/*
 * QuickJS Javascript Engine
 *
 * Copyright (c) 2017-2021 Fabrice Bellard
 * Copyright (c) 2017-2021 Charlie Gordon
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 */
#ifndef QUICKJS_H
#define QUICKJS_H

#include <stdio.h>
#include <stdint.h>
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(__GNUC__) || defined(__clang__)
#define js_likely(x)          __builtin_expect(!!(x), 1)
#define js_unlikely(x)        __builtin_expect(!!(x), 0)
#define js_force_inline       inline __attribute__((always_inline))
#define __js_printf_like(f, a)   __attribute__((format(printf, f, a)))
#else
#define js_likely(x)     (x)
#define js_unlikely(x)   (x)
#define js_force_inline  inline
#define __js_printf_like(a, b)
#endif

#define JS_BOOL int

typedef struct JSRuntime JSRuntime;
typedef struct JSContext JSContext;
typedef struct JSClass JSClass;
typedef uint32_t JSClassID;
typedef uint32_t JSAtom;

#if INTPTR_MAX >= INT64_MAX
#define JS_PTR64
#define JS_PTR64_DEF(a) a
#else
#define JS_PTR64_DEF(a)
#endif

#ifndef JS_PTR64
#define JS_NAN_BOXING
#endif

#if defined(__SIZEOF_INT128__) && (INTPTR_MAX >= INT64_MAX)
#define JS_LIMB_BITS 64
#else
#define JS_LIMB_BITS 32
#endif

#define JS_SHORT_BIG_INT_BITS JS_LIMB_BITS
    
enum {
    /* all tags with a reference count are negative */
    JS_TAG_FIRST       = -9, /* first negative tag */
    JS_TAG_BIG_INT     = -9,
    JS_TAG_SYMBOL      = -8,
    JS_TAG_STRING      = -7,
    JS_TAG_STRING_ROPE = -6,
    JS_TAG_MODULE      = -3, /* used internally */
    JS_TAG_FUNCTION_BYTECODE = -2, /* used internally */
    JS_TAG_OBJECT      = -1,

    JS_TAG_INT         = 0,
    JS_TAG_BOOL        = 1,
    JS_TAG_NULL        = 2,
    JS_TAG_UNDEFINED   = 3,
    JS_TAG_UNINITIALIZED = 4,
    JS_TAG_CATCH_OFFSET = 5,
    JS_TAG_EXCEPTION   = 6,
    JS_TAG_SHORT_BIG_INT = 7,
    JS_TAG_FLOAT64     = 8,
    /* any larger tag is FLOAT64 if JS_NAN_BOXING */
};

/* must match the layout of 'JSMallocBlockHeader' */
typedef struct JSRefCountHeader {
    int ref_count;
} JSRefCountHeader;

#define JS_FLOAT64_NAN NAN

#ifdef CONFIG_CHECK_JSVALUE
/* JSValue consistency : it is not possible to run the code in this
   mode, but it is useful to detect simple reference counting
   errors. It would be interesting to modify a static C analyzer to
   handle specific annotations (clang has such annotations but only
   for objective C) */
typedef struct __JSValue *JSValue;
typedef const struct __JSValue *JSValueConst;

#define JS_VALUE_GET_TAG(v) (int)((uintptr_t)(v) & 0xf)
/* same as JS_VALUE_GET_TAG, but return JS_TAG_FLOAT64 with NaN boxing */
#define JS_VALUE_GET_NORM_TAG(v) JS_VALUE_GET_TAG(v)
#define JS_VALUE_GET_INT(v) (int)((intptr_t)(v) >> 4)
#define JS_VALUE_GET_BOOL(v) JS_VALUE_GET_INT(v)
#define JS_VALUE_GET_FLOAT64(v) (double)JS_VALUE_GET_INT(v)
#define JS_VALUE_GET_SHORT_BIG_INT(v) JS_VALUE_GET_INT(v)
#define JS_VALUE_GET_PTR(v) (void *)((intptr_t)(v) & ~0xf)

#define JS_MKVAL(tag, val) (JSValue)(intptr_t)(((val) << 4) | (tag))
#define JS_MKPTR(tag, p) (JSValue)((intptr_t)(p) | (tag))

#define JS_TAG_IS_FLOAT64(tag) ((unsigned)(tag) == JS_TAG_FLOAT64)

#define JS_NAN JS_MKVAL(JS_TAG_FLOAT64, 1)

static inline JSValue __JS_NewFloat64(JSContext *ctx, double d)
{
    return JS_MKVAL(JS_TAG_FLOAT64, (int)d);
}

static inline JS_BOOL JS_VALUE_IS_NAN(JSValue v)
{
    return 0;
}

static inline JSValue __JS_NewShortBigInt(JSContext *ctx, int32_t d)
{
    return JS_MKVAL(JS_TAG_SHORT_BIG_INT, d);
}

#elif defined(JS_NAN_BOXING)

typedef uint64_t JSValue;

#define JSValueConst JSValue

#define JS_VALUE_GET_TAG(v) (int)((v) >> 32)
#define JS_VALUE_GET_INT(v) (int)(v)
#define JS_VALUE_GET_BOOL(v) (int)(v)
#define JS_VALUE_GET_SHORT_BIG_INT(v) (int)(v)
#define JS_VALUE_GET_PTR(v) (void *)(intptr_t)(v)

#define JS_MKVAL(tag, val) (((uint64_t)(tag) << 32) | (uint32_t)(val))
#define JS_MKPTR(tag, ptr) (((uint64_t)(tag) << 32) | (uintptr_t)(ptr))

#define JS_FLOAT64_TAG_ADDEND (0x7ff80000 - JS_TAG_FIRST + 1) /* quiet NaN encoding */

static inline double JS_VALUE_GET_FLOAT64(JSValue v)
{
    union {
        JSValue v;
        double d;
    } u;
    u.v = v;
    u.v += (uint64_t)JS_FLOAT64_TAG_ADDEND << 32;
    return u.d;
}

#define JS_NAN (0x7ff8000000000000 - ((uint64_t)JS_FLOAT64_TAG_ADDEND << 32))

static inline JSValue __JS_NewFloat64(JSContext *ctx, double d)
{
    union {
        double d;
        uint64_t u64;
    } u;
    JSValue v;
    u.d = d;
    /* normalize NaN */
    if (js_unlikely((u.u64 & 0x7fffffffffffffff) > 0x7ff0000000000000))
        v = JS_NAN;
    else
        v = u.u64 - ((uint64_t)JS_FLOAT64_TAG_ADDEND << 32);
    return v;
}

#define JS_TAG_IS_FLOAT64(tag) ((unsigned)((tag) - JS_TAG_FIRST) >= (JS_TAG_FLOAT64 - JS_TAG_FIRST))

/* same as JS_VALUE_GET_TAG, but return JS_TAG_FLOAT64 with NaN boxing */
static inline int JS_VALUE_GET_NORM_TAG(JSValue v)
{
    uint32_t tag;
    tag = JS_VALUE_GET_TAG(v);
    if (JS_TAG_IS_FLOAT64(tag))
        return JS_TAG_FLOAT64;
    else
        return tag;
}

static inline JS_BOOL JS_VALUE_IS_NAN(JSValue v)
{
    uint32_t tag;
    tag = JS_VALUE_GET_TAG(v);
    return tag == (JS_NAN >> 32);
}

static inline JSValue __JS_NewShortBigInt(JSContext *ctx, int32_t d)
{
    return JS_MKVAL(JS_TAG_SHORT_BIG_INT, d);
}

#else /* !JS_NAN_BOXING */

typedef union JSValueUnion {
    uint64_t uint64;
    double float64;
    void *ptr;
#if JS_SHORT_BIG_INT_BITS == 32
    int32_t short_big_int;
#else
    int64_t short_big_int;
#endif
} JSValueUnion;

typedef struct JSValue {
    JSValueUnion u;
    int64_t tag;
} JSValue;

#define JSValueConst JSValue

#define JS_VALUE_GET_TAG(v) ((int32_t)(v).tag)
/* same as JS_VALUE_GET_TAG, but return JS_TAG_FLOAT64 with NaN boxing */
#define JS_VALUE_GET_NORM_TAG(v) JS_VALUE_GET_TAG(v)
#define JS_VALUE_GET_INT(v) ((int)(v).u.uint64)
#define JS_VALUE_GET_BOOL(v) ((int)(v).u.uint64)
#define JS_VALUE_GET_FLOAT64(v) ((v).u.float64)
#define JS_VALUE_GET_SHORT_BIG_INT(v) ((v).u.short_big_int)
#define JS_VALUE_GET_PTR(v) ((v).u.ptr)

/* avoid uninitialized data by using a 64 bit field even if only 32
   bits are needed because some compilers generate slower code */
#define JS_MKVAL(tag, val) (JSValue){ (JSValueUnion){ .uint64 = (uint32_t)(val) }, tag }
#define JS_MKPTR(tag, p) (JSValue){ (JSValueUnion){ .ptr = p }, tag }

#define JS_TAG_IS_FLOAT64(tag) ((unsigned)(tag) == JS_TAG_FLOAT64)

#define JS_NAN (JSValue){ .u.float64 = JS_FLOAT64_NAN, JS_TAG_FLOAT64 }

static inline JSValue __JS_NewFloat64(JSContext *ctx, double d)
{
    JSValue v;
    v.tag = JS_TAG_FLOAT64;
    v.u.float64 = d;
    return v;
}

static inline JS_BOOL JS_VALUE_IS_NAN(JSValue v)
{
    union {
        double d;
        uint64_t u64;
    } u;
    if (v.tag != JS_TAG_FLOAT64)
        return 0;
    u.d = v.u.float64;
    return (u.u64 & 0x7fffffffffffffff) > 0x7ff0000000000000;
}

static inline JSValue __JS_NewShortBigInt(JSContext *ctx, int64_t d)
{
    JSValue v;
    v.tag = JS_TAG_SHORT_BIG_INT;
    v.u.short_big_int = d;
    return v;
}

#endif /* !JS_NAN_BOXING */

#define JS_VALUE_IS_BOTH_INT(v1, v2) ((JS_VALUE_GET_TAG(v1) | JS_VALUE_GET_TAG(v2)) == 0)
#define JS_VALUE_IS_BOTH_FLOAT(v1, v2) (JS_TAG_IS_FLOAT64(JS_VALUE_GET_TAG(v1)) && JS_TAG_IS_FLOAT64(JS_VALUE_GET_TAG(v2)))

#define JS_VALUE_HAS_REF_COUNT(v) ((unsigned)JS_VALUE_GET_TAG(v) >= (unsigned)JS_TAG_FIRST)

/* special values */
#define JS_NULL      JS_MKVAL(JS_TAG_NULL, 0)
#define JS_UNDEFINED JS_MKVAL(JS_TAG_UNDEFINED, 0)
#define JS_FALSE     JS_MKVAL(JS_TAG_BOOL, 0)
#define JS_TRUE      JS_MKVAL(JS_TAG_BOOL, 1)
#define JS_EXCEPTION JS_MKVAL(JS_TAG_EXCEPTION, 0)
#define JS_UNINITIALIZED JS_MKVAL(JS_TAG_UNINITIALIZED, 0)

/* flags for object properties */
#define JS_PROP_CONFIGURABLE  (1 << 0)
#define JS_PROP_WRITABLE      (1 << 1)
#define JS_PROP_ENUMERABLE    (1 << 2)
#define JS_PROP_C_W_E         (JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE | JS_PROP_ENUMERABLE)
#define JS_PROP_LENGTH        (1 << 3) /* used internally in Arrays */
#define JS_PROP_TMASK         (3 << 4) /* mask for NORMAL, GETSET, VARREF, AUTOINIT */
#define JS_PROP_NORMAL         (0 << 4)
#define JS_PROP_GETSET         (1 << 4)
#define JS_PROP_VARREF         (2 << 4) /* used internally */
#define JS_PROP_AUTOINIT       (3 << 4) /* used internally */

/* flags for JS_DefineProperty */
#define JS_PROP_HAS_SHIFT        8
#define JS_PROP_HAS_CONFIGURABLE (1 << 8)
#define JS_PROP_HAS_WRITABLE     (1 << 9)
#define JS_PROP_HAS_ENUMERABLE   (1 << 10)
#define JS_PROP_HAS_GET          (1 << 11)
#define JS_PROP_HAS_SET          (1 << 12)
#define JS_PROP_HAS_VALUE        (1 << 13)

/* throw an exception if false would be returned
   (JS_DefineProperty/JS_SetProperty) */
#define JS_PROP_THROW            (1 << 14)
/* throw an exception if false would be returned in strict mode
   (JS_SetProperty) */
#define JS_PROP_THROW_STRICT     (1 << 15)

#define JS_PROP_NO_EXOTIC        (1 << 16) /* internal use */

#ifndef JS_DEFAULT_STACK_SIZE
#define JS_DEFAULT_STACK_SIZE (1024 * 1024)
#endif

/* JS_Eval() flags */
#define JS_EVAL_TYPE_GLOBAL   (0 << 0) /* global code (default) */
#define JS_EVAL_TYPE_MODULE   (1 << 0) /* module code */
#define JS_EVAL_TYPE_DIRECT   (2 << 0) /* direct call (internal use) */
#define JS_EVAL_TYPE_INDIRECT (3 << 0) /* indirect call (internal use) */
#define JS_EVAL_TYPE_MASK     (3 << 0)

#define JS_EVAL_FLAG_STRICT   (1 << 3) /* force 'strict' mode */
/* compile but do not run. The result is an object with a
   JS_TAG_FUNCTION_BYTECODE or JS_TAG_MODULE tag. It can be executed
   with JS_EvalFunction(). */
#define JS_EVAL_FLAG_COMPILE_ONLY (1 << 5)
/* don't include the stack frames before this eval in the Error() backtraces */
#define JS_EVAL_FLAG_BACKTRACE_BARRIER (1 << 6)
/* allow top-level await in normal script. JS_Eval() returns a
   promise. Only allowed with JS_EVAL_TYPE_GLOBAL */
#define JS_EVAL_FLAG_ASYNC (1 << 7)
/* Servo: with JS_EVAL_TYPE_MODULE and JS_EVAL_FLAG_COMPILE_ONLY, do not load the
   requested modules; JS_LinkModule() resolves them later */
#define JS_EVAL_FLAG_NO_RESOLVE (1 << 8)
/* Servo: the script's errors are muted (HTML "muted errors"); eval and Function
   code inherit it */
#define JS_EVAL_FLAG_MUTED_ERRORS (1 << 9)

typedef JSValue JSCFunction(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv);
typedef JSValue JSCFunctionMagic(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv, int magic);
typedef JSValue JSCFunctionData(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv, int magic, JSValue *func_data);

typedef struct JSMallocState {
    size_t malloc_count;
    size_t malloc_size;
    size_t malloc_limit;
    void *opaque; /* user opaque */
} JSMallocState;

typedef struct JSMallocFunctions {
    void *(*js_malloc)(JSMallocState *s, size_t size);
    void (*js_free)(JSMallocState *s, void *ptr);
    void *(*js_realloc)(JSMallocState *s, void *ptr, size_t size);
    size_t (*js_malloc_usable_size)(const void *ptr);
} JSMallocFunctions;

typedef struct JSGCObjectHeader JSGCObjectHeader;

JSRuntime *JS_NewRuntime(void);
/* info lifetime must exceed that of rt */
void JS_SetRuntimeInfo(JSRuntime *rt, const char *info);
void JS_SetMemoryLimit(JSRuntime *rt, size_t limit);
void JS_SetGCThreshold(JSRuntime *rt, size_t gc_threshold);
/* use 0 to disable maximum stack size check */
void JS_SetMaxStackSize(JSRuntime *rt, size_t stack_size);
/* should be called when changing thread to update the stack top value
   used to check stack overflow. */
void JS_UpdateStackTop(JSRuntime *rt);
JSRuntime *JS_NewRuntime2(const JSMallocFunctions *mf, void *opaque);
void JS_FreeRuntime(JSRuntime *rt);
void *JS_GetRuntimeOpaque(JSRuntime *rt);
void JS_SetRuntimeOpaque(JSRuntime *rt, void *opaque);
typedef void JS_MarkFunc(JSRuntime *rt, JSGCObjectHeader *gp);
void JS_MarkValue(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func);
void JS_RunGC(JSRuntime *rt);
JS_BOOL JS_IsLiveObject(JSRuntime *rt, JSValueConst obj);

JSContext *JS_NewContext(JSRuntime *rt);
void JS_FreeContext(JSContext *s);
JSContext *JS_DupContext(JSContext *ctx);
void *JS_GetContextOpaque(JSContext *ctx);
void JS_SetContextOpaque(JSContext *ctx, void *opaque);
/* Servo: called when a context is freed, after it released its values */
typedef void JSContextFinalizer(JSContext *ctx);
void JS_SetContextFinalizer(JSRuntime *rt, JSContextFinalizer *finalizer);
/* Servo: marks a context reference (JS_DupContext) held by an object, from its
   class gc_mark */
void JS_MarkContextRef(JSRuntime *rt, JSContext *ctx, JS_MarkFunc *mark_func);
JSRuntime *JS_GetRuntime(JSContext *ctx);
void JS_SetClassProto(JSContext *ctx, JSClassID class_id, JSValue obj);
JSValue JS_GetClassProto(JSContext *ctx, JSClassID class_id);

/* the following functions are used to select the intrinsic object to
   save memory */
JSContext *JS_NewContextRaw(JSRuntime *rt);
int JS_AddIntrinsicBaseObjects(JSContext *ctx);
int JS_AddIntrinsicDate(JSContext *ctx);
int JS_AddIntrinsicEval(JSContext *ctx);
int JS_AddIntrinsicStringNormalize(JSContext *ctx);
void JS_AddIntrinsicRegExpCompiler(JSContext *ctx);
int JS_AddIntrinsicRegExp(JSContext *ctx);
int JS_AddIntrinsicJSON(JSContext *ctx);
int JS_AddIntrinsicProxy(JSContext *ctx);
int JS_AddIntrinsicMapSet(JSContext *ctx);
int JS_AddIntrinsicTypedArrays(JSContext *ctx);
int JS_AddIntrinsicPromise(JSContext *ctx);
int JS_AddIntrinsicWeakRef(JSContext *ctx);

JSValue js_string_codePointRange(JSContext *ctx, JSValueConst this_val,
                                 int argc, JSValueConst *argv);

void *js_malloc_rt(JSRuntime *rt, size_t size);
void js_free_rt(JSRuntime *rt, void *ptr);
void *js_realloc_rt(JSRuntime *rt, void *ptr, size_t size);
size_t js_malloc_usable_size_rt(JSRuntime *rt, const void *ptr);
void *js_mallocz_rt(JSRuntime *rt, size_t size);

void *js_malloc(JSContext *ctx, size_t size);
void js_free(JSContext *ctx, void *ptr);
void *js_realloc(JSContext *ctx, void *ptr, size_t size);
size_t js_malloc_usable_size(JSContext *ctx, const void *ptr);
void *js_realloc2(JSContext *ctx, void *ptr, size_t size, size_t *pslack);
void *js_mallocz(JSContext *ctx, size_t size);
char *js_strdup(JSContext *ctx, const char *str);
char *js_strndup(JSContext *ctx, const char *s, size_t n);

typedef struct JSMemoryUsage {
    int64_t malloc_size, malloc_limit, memory_used_size;
    int64_t malloc_count;
    int64_t memory_used_count;
    int64_t atom_count, atom_size;
    int64_t str_count, str_size;
    int64_t obj_count, obj_size;
    int64_t prop_count, prop_size;
    int64_t shape_count, shape_size;
    int64_t js_func_count, js_func_size, js_func_code_size;
    int64_t js_func_pc2line_count, js_func_pc2line_size;
    int64_t c_func_count, array_count;
    int64_t fast_array_count, fast_array_elements;
    int64_t binary_object_count, binary_object_size;
} JSMemoryUsage;

void JS_ComputeMemoryUsage(JSRuntime *rt, JSMemoryUsage *s);
void JS_DumpMemoryUsage(FILE *fp, const JSMemoryUsage *s, JSRuntime *rt);

/* atom support */
#define JS_ATOM_NULL 0

JSAtom JS_NewAtomLen(JSContext *ctx, const char *str, size_t len);
JSAtom JS_NewAtom(JSContext *ctx, const char *str);
JSAtom JS_NewAtomUInt32(JSContext *ctx, uint32_t n);
JSAtom JS_DupAtom(JSContext *ctx, JSAtom v);
void JS_FreeAtom(JSContext *ctx, JSAtom v);
void JS_FreeAtomRT(JSRuntime *rt, JSAtom v);
JSValue JS_AtomToValue(JSContext *ctx, JSAtom atom);
JSValue JS_AtomToString(JSContext *ctx, JSAtom atom);
const char *JS_AtomToCStringLen(JSContext *ctx, size_t *plen, JSAtom atom);
static inline const char *JS_AtomToCString(JSContext *ctx, JSAtom atom)
{
    return JS_AtomToCStringLen(ctx, NULL, atom);
}
JSAtom JS_ValueToAtom(JSContext *ctx, JSValueConst val);

/* object class support */

typedef struct JSPropertyEnum {
    JS_BOOL is_enumerable;
    JSAtom atom;
} JSPropertyEnum;

typedef struct JSPropertyDescriptor {
    int flags;
    JSValue value;
    JSValue getter;
    JSValue setter;
} JSPropertyDescriptor;

typedef struct JSClassExoticMethods {
    /* Return -1 if exception (can only happen in case of Proxy object),
       FALSE if the property does not exists, TRUE if it exists. If 1 is
       returned, the property descriptor 'desc' is filled if != NULL. */
    int (*get_own_property)(JSContext *ctx, JSPropertyDescriptor *desc,
                             JSValueConst obj, JSAtom prop);
    /* '*ptab' should hold the '*plen' property keys. Return 0 if OK,
       -1 if exception. The 'is_enumerable' field is ignored.
    */
    int (*get_own_property_names)(JSContext *ctx, JSPropertyEnum **ptab,
                                  uint32_t *plen,
                                  JSValueConst obj);
    /* return < 0 if exception, or TRUE/FALSE */
    int (*delete_property)(JSContext *ctx, JSValueConst obj, JSAtom prop);
    /* return < 0 if exception or TRUE/FALSE */
    int (*define_own_property)(JSContext *ctx, JSValueConst this_obj,
                               JSAtom prop, JSValueConst val,
                               JSValueConst getter, JSValueConst setter,
                               int flags);
    /* The following methods can be emulated with the previous ones,
       so they are usually not needed */
    /* return < 0 if exception or TRUE/FALSE */
    int (*has_property)(JSContext *ctx, JSValueConst obj, JSAtom atom);
    JSValue (*get_property)(JSContext *ctx, JSValueConst obj, JSAtom atom,
                            JSValueConst receiver);
    /* return < 0 if exception or TRUE/FALSE */
    int (*set_property)(JSContext *ctx, JSValueConst obj, JSAtom atom,
                        JSValueConst value, JSValueConst receiver, int flags);

    /* To get a consistent object behavior when get_prototype != NULL,
       get_property, set_property and set_prototype must be != NULL
       and the object must be created with a JS_NULL prototype. */
    JSValue (*get_prototype)(JSContext *ctx, JSValueConst obj);
    /* return < 0 if exception or TRUE/FALSE */
    int (*set_prototype)(JSContext *ctx, JSValueConst obj, JSValueConst proto_val);
    /* return < 0 if exception or TRUE/FALSE */
    int (*is_extensible)(JSContext *ctx, JSValueConst obj);
    /* return < 0 if exception or TRUE/FALSE */
    int (*prevent_extensions)(JSContext *ctx, JSValueConst obj);
} JSClassExoticMethods;

typedef void JSClassFinalizer(JSRuntime *rt, JSValue val);
typedef void JSClassGCMark(JSRuntime *rt, JSValueConst val,
                           JS_MarkFunc *mark_func);
#define JS_CALL_FLAG_CONSTRUCTOR (1 << 0)
typedef JSValue JSClassCall(JSContext *ctx, JSValueConst func_obj,
                            JSValueConst this_val, int argc, JSValueConst *argv,
                            int flags);

typedef struct JSClassDef {
    const char *class_name;
    JSClassFinalizer *finalizer;
    JSClassGCMark *gc_mark;
    /* if call != NULL, the object is a function. If (flags &
       JS_CALL_FLAG_CONSTRUCTOR) != 0, the function is called as a
       constructor. In this case, 'this_val' is new.target. A
       constructor call only happens if the object constructor bit is
       set (see JS_SetConstructorBit()). */
    JSClassCall *call;
    /* XXX: suppress this indirection ? It is here only to save memory
       because only a few classes need these methods */
    JSClassExoticMethods *exotic;
} JSClassDef;

#define JS_INVALID_CLASS_ID 0
JSClassID JS_NewClassID(JSClassID *pclass_id);
/* Returns the class ID if `v` is an object, otherwise returns JS_INVALID_CLASS_ID. */
JSClassID JS_GetClassID(JSValue v);
int JS_NewClass(JSRuntime *rt, JSClassID class_id, const JSClassDef *class_def);
int JS_IsRegisteredClass(JSRuntime *rt, JSClassID class_id);

/* value handling */

static js_force_inline JSValue JS_NewBool(JSContext *ctx, JS_BOOL val)
{
    return JS_MKVAL(JS_TAG_BOOL, (val != 0));
}

static js_force_inline JSValue JS_NewInt32(JSContext *ctx, int32_t val)
{
    return JS_MKVAL(JS_TAG_INT, val);
}

static js_force_inline JSValue JS_NewCatchOffset(JSContext *ctx, int32_t val)
{
    return JS_MKVAL(JS_TAG_CATCH_OFFSET, val);
}

static js_force_inline JSValue JS_NewInt64(JSContext *ctx, int64_t val)
{
    JSValue v;
    if (val == (int32_t)val) {
        v = JS_NewInt32(ctx, val);
    } else {
        v = __JS_NewFloat64(ctx, val);
    }
    return v;
}

static js_force_inline JSValue JS_NewUint32(JSContext *ctx, uint32_t val)
{
    JSValue v;
    if (val <= 0x7fffffff) {
        v = JS_NewInt32(ctx, val);
    } else {
        v = __JS_NewFloat64(ctx, val);
    }
    return v;
}

JSValue JS_NewBigInt64(JSContext *ctx, int64_t v);
JSValue JS_NewBigUint64(JSContext *ctx, uint64_t v);

static js_force_inline JSValue JS_NewFloat64(JSContext *ctx, double d)
{
    int32_t val;
    union {
        double d;
        uint64_t u;
    } u, t;
    if (d >= INT32_MIN && d <= INT32_MAX) {
        u.d = d;
        val = (int32_t)d;
        t.d = val;
        /* -0 cannot be represented as integer, so we compare the bit
           representation */
        if (u.u == t.u)
            return JS_MKVAL(JS_TAG_INT, val);
    }
    return __JS_NewFloat64(ctx, d);
}

static inline JS_BOOL JS_IsNumber(JSValueConst v)
{
    int tag = JS_VALUE_GET_TAG(v);
    return tag == JS_TAG_INT || JS_TAG_IS_FLOAT64(tag);
}

static inline JS_BOOL JS_IsBigInt(JSContext *ctx, JSValueConst v)
{
    int tag = JS_VALUE_GET_TAG(v);
    return tag == JS_TAG_BIG_INT || tag == JS_TAG_SHORT_BIG_INT;
}

static inline JS_BOOL JS_IsBool(JSValueConst v)
{
    return JS_VALUE_GET_TAG(v) == JS_TAG_BOOL;
}

static inline JS_BOOL JS_IsNull(JSValueConst v)
{
    return JS_VALUE_GET_TAG(v) == JS_TAG_NULL;
}

static inline JS_BOOL JS_IsUndefined(JSValueConst v)
{
    return JS_VALUE_GET_TAG(v) == JS_TAG_UNDEFINED;
}

static inline JS_BOOL JS_IsException(JSValueConst v)
{
    return js_unlikely(JS_VALUE_GET_TAG(v) == JS_TAG_EXCEPTION);
}

static inline JS_BOOL JS_IsUninitialized(JSValueConst v)
{
    return js_unlikely(JS_VALUE_GET_TAG(v) == JS_TAG_UNINITIALIZED);
}

static inline JS_BOOL JS_IsString(JSValueConst v)
{
    return JS_VALUE_GET_TAG(v) == JS_TAG_STRING ||
        JS_VALUE_GET_TAG(v) == JS_TAG_STRING_ROPE;
}

static inline JS_BOOL JS_IsSymbol(JSValueConst v)
{
    return JS_VALUE_GET_TAG(v) == JS_TAG_SYMBOL;
}

static inline JS_BOOL JS_IsObject(JSValueConst v)
{
    return JS_VALUE_GET_TAG(v) == JS_TAG_OBJECT;
}

JSValue JS_Throw(JSContext *ctx, JSValue obj);
void JS_SetUncatchableException(JSContext *ctx, JS_BOOL flag);
JSValue JS_GetException(JSContext *ctx);
JS_BOOL JS_HasException(JSContext *ctx);
/* Servo: whether the pending exception is uncatchable (e.g. interrupted script) */
JS_BOOL JS_HasUncatchableException(JSContext *ctx);
/* Servo: where the last exception was thrown, if in a bytecode function (the
   innermost such frame it unwound). 'filename' is valid until the next throw. */
JS_BOOL JS_GetExceptionLocation(JSContext *ctx, JSAtom *pfilename, int *pline_num,
                                int *pcol_num);
/* Servo: the frames of the current call stack, innermost first, until 'cb' returns
   non-zero. 'name' is the function's own "name" data property or JS_UNDEFINED (for
   top-level code), borrowed. Native frames have 'is_native' set and no location.
   'module' and 'script_private' are the frame's script or module as for
   JSDynamicImportHook. */
typedef struct JSStackFrameInfo {
    const void *frame; /* identifies the frame while it is live */
    JS_BOOL is_native;
    JSContext *realm; /* realm of a bytecode function */
    struct JSModuleDef *module;
    JSValueConst script_private; /* borrowed */
    JS_BOOL muted_errors; /* see JS_EVAL_FLAG_MUTED_ERRORS */
    JSValueConst name;
    JSAtom filename; /* JS_ATOM_NULL if unknown */
    int line_num; /* 1-based, 0 if unknown */
    int col_num; /* 1-based, 0 if unknown */
} JSStackFrameInfo;
typedef int JSStackFrameCallback(void *opaque, const JSStackFrameInfo *info);
void JS_IterateStackFrames(JSContext *ctx, JSStackFrameCallback *cb, void *opaque);
JS_BOOL JS_IsError(JSContext *ctx, JSValueConst val);
JSValue JS_NewError(JSContext *ctx);
JSValue __js_printf_like(2, 3) JS_ThrowSyntaxError(JSContext *ctx, const char *fmt, ...);
JSValue __js_printf_like(2, 3) JS_ThrowTypeError(JSContext *ctx, const char *fmt, ...);
JSValue __js_printf_like(2, 3) JS_ThrowReferenceError(JSContext *ctx, const char *fmt, ...);
JSValue __js_printf_like(2, 3) JS_ThrowRangeError(JSContext *ctx, const char *fmt, ...);
JSValue __js_printf_like(2, 3) JS_ThrowInternalError(JSContext *ctx, const char *fmt, ...);
JSValue JS_ThrowOutOfMemory(JSContext *ctx);

void __JS_FreeValue(JSContext *ctx, JSValue v);

static inline JSRefCountHeader *__js_rc(void *ptr)
{
    return (JSRefCountHeader *)((uint32_t *)ptr - 1);
}

static inline void JS_FreeValue(JSContext *ctx, JSValue v)
{
    if (JS_VALUE_HAS_REF_COUNT(v)) {
        JSRefCountHeader *p = __js_rc(JS_VALUE_GET_PTR(v));
        if (--p->ref_count <= 0) {
            __JS_FreeValue(ctx, v);
        }
    }
}
void __JS_FreeValueRT(JSRuntime *rt, JSValue v);
static inline void JS_FreeValueRT(JSRuntime *rt, JSValue v)
{
    if (JS_VALUE_HAS_REF_COUNT(v)) {
        JSRefCountHeader *p = __js_rc(JS_VALUE_GET_PTR(v));
        if (--p->ref_count <= 0) {
            __JS_FreeValueRT(rt, v);
        }
    }
}

static inline JSValue JS_DupValue(JSContext *ctx, JSValueConst v)
{
    if (JS_VALUE_HAS_REF_COUNT(v)) {
        JSRefCountHeader *p = __js_rc(JS_VALUE_GET_PTR(v));
        p->ref_count++;
    }
    return (JSValue)v;
}

static inline JSValue JS_DupValueRT(JSRuntime *rt, JSValueConst v)
{
    if (JS_VALUE_HAS_REF_COUNT(v)) {
        JSRefCountHeader *p = __js_rc(JS_VALUE_GET_PTR(v));
        p->ref_count++;
    }
    return (JSValue)v;
}

JS_BOOL JS_StrictEq(JSContext *ctx, JSValueConst op1, JSValueConst op2);
JS_BOOL JS_SameValue(JSContext *ctx, JSValueConst op1, JSValueConst op2);
JS_BOOL JS_SameValueZero(JSContext *ctx, JSValueConst op1, JSValueConst op2);

int JS_ToBool(JSContext *ctx, JSValueConst val); /* return -1 for JS_EXCEPTION */
int JS_ToInt32(JSContext *ctx, int32_t *pres, JSValueConst val);
static inline int JS_ToUint32(JSContext *ctx, uint32_t *pres, JSValueConst val)
{
    return JS_ToInt32(ctx, (int32_t*)pres, val);
}
int JS_ToInt64(JSContext *ctx, int64_t *pres, JSValueConst val);
int JS_ToIndex(JSContext *ctx, uint64_t *plen, JSValueConst val);
int JS_ToFloat64(JSContext *ctx, double *pres, JSValueConst val);
/* return an exception if 'val' is a Number */
int JS_ToBigInt64(JSContext *ctx, int64_t *pres, JSValueConst val);
/* same as JS_ToInt64() but allow BigInt */
int JS_ToInt64Ext(JSContext *ctx, int64_t *pres, JSValueConst val);
JSValue JS_NewBigInt128(JSContext *ctx, uint64_t low, uint64_t high);
int JS_ToBigInt128(JSContext *ctx, uint64_t *plow, uint64_t *phigh, JSValueConst val);
int JS_ToBigInt128Sat(JSContext *ctx, uint64_t *plow, uint64_t *phigh, JSValueConst val);

JSValue JS_NewStringLen(JSContext *ctx, const char *str1, size_t len1);
/* Servo: Latin-1 and UTF-16 strings, and direct read access to the characters of a
   JS_TAG_STRING value (8-bit if *pwide is 0, 16-bit otherwise). */
JSValue JS_NewStringLatin1(JSContext *ctx, const uint8_t *buf, size_t len);
JSValue JS_NewStringUTF16(JSContext *ctx, const uint16_t *buf, size_t len);
size_t JS_GetStringData(JSValueConst str, const void **pdata, int *pwide);
static inline JSValue JS_NewString(JSContext *ctx, const char *str)
{
    return JS_NewStringLen(ctx, str, strlen(str));
}
JSValue JS_NewAtomString(JSContext *ctx, const char *str);
JSValue JS_ToString(JSContext *ctx, JSValueConst val);
JSValue JS_ToPropertyKey(JSContext *ctx, JSValueConst val);
const char *JS_ToCStringLen2(JSContext *ctx, size_t *plen, JSValueConst val1, JS_BOOL cesu8);
static inline const char *JS_ToCStringLen(JSContext *ctx, size_t *plen, JSValueConst val1)
{
    return JS_ToCStringLen2(ctx, plen, val1, 0);
}
static inline const char *JS_ToCString(JSContext *ctx, JSValueConst val1)
{
    return JS_ToCStringLen2(ctx, NULL, val1, 0);
}
void JS_FreeCString(JSContext *ctx, const char *ptr);

JSValue JS_NewSymbol(JSContext *ctx, const char *description, JS_BOOL is_global);

JSValue JS_NewObjectProtoClass(JSContext *ctx, JSValueConst proto, JSClassID class_id);
JSValue JS_NewObjectClass(JSContext *ctx, int class_id);
JSValue JS_NewObjectProto(JSContext *ctx, JSValueConst proto);
JSValue JS_NewObject(JSContext *ctx);

JS_BOOL JS_IsFunction(JSContext* ctx, JSValueConst val);
JS_BOOL JS_IsConstructor(JSContext* ctx, JSValueConst val);
JS_BOOL JS_SetConstructorBit(JSContext *ctx, JSValueConst func_obj, JS_BOOL val);

JSValue JS_NewArray(JSContext *ctx);
int JS_IsArray(JSContext *ctx, JSValueConst val);

JSValue JS_NewDate(JSContext *ctx, double epoch_ms);

JSValue JS_GetPropertyInternal(JSContext *ctx, JSValueConst obj,
                               JSAtom prop, JSValueConst receiver,
                               JS_BOOL throw_ref_error);
static js_force_inline JSValue JS_GetProperty(JSContext *ctx, JSValueConst this_obj,
                                              JSAtom prop)
{
    return JS_GetPropertyInternal(ctx, this_obj, prop, this_obj, 0);
}
JSValue JS_GetPropertyStr(JSContext *ctx, JSValueConst this_obj,
                          const char *prop);
JSValue JS_GetPropertyUint32(JSContext *ctx, JSValueConst this_obj,
                             uint32_t idx);

int JS_SetPropertyInternal(JSContext *ctx, JSValueConst obj,
                           JSAtom prop, JSValue val, JSValueConst this_obj,
                           int flags);
static inline int JS_SetProperty(JSContext *ctx, JSValueConst this_obj,
                                 JSAtom prop, JSValue val)
{
    return JS_SetPropertyInternal(ctx, this_obj, prop, val, this_obj, JS_PROP_THROW);
}
int JS_SetPropertyUint32(JSContext *ctx, JSValueConst this_obj,
                         uint32_t idx, JSValue val);
int JS_SetPropertyInt64(JSContext *ctx, JSValueConst this_obj,
                        int64_t idx, JSValue val);
int JS_SetPropertyStr(JSContext *ctx, JSValueConst this_obj,
                      const char *prop, JSValue val);
int JS_HasProperty(JSContext *ctx, JSValueConst this_obj, JSAtom prop);
int JS_IsExtensible(JSContext *ctx, JSValueConst obj);
int JS_PreventExtensions(JSContext *ctx, JSValueConst obj);
int JS_DeleteProperty(JSContext *ctx, JSValueConst obj, JSAtom prop, int flags);
int JS_SetPrototype(JSContext *ctx, JSValueConst obj, JSValueConst proto_val);
JSValue JS_GetPrototype(JSContext *ctx, JSValueConst val);
/* Servo: prototype access without exotic hooks, immutable prototypes and the realm
   of a function (for the mozjs shim). */
JSValue JS_GetStaticPrototype(JSContext *ctx, JSValueConst obj);
void JS_SetImmutablePrototype(JSContext *ctx, JSValueConst obj);
JSContext *JS_GetFunctionRealm(JSContext *ctx, JSValueConst func_obj);

#define JS_GPN_STRING_MASK  (1 << 0)
#define JS_GPN_SYMBOL_MASK  (1 << 1)
#define JS_GPN_PRIVATE_MASK (1 << 2)
/* only include the enumerable properties */
#define JS_GPN_ENUM_ONLY    (1 << 4)
/* set theJSPropertyEnum.is_enumerable field */
#define JS_GPN_SET_ENUM     (1 << 5)

int JS_GetOwnPropertyNames(JSContext *ctx, JSPropertyEnum **ptab,
                           uint32_t *plen, JSValueConst obj, int flags);
void JS_FreePropertyEnum(JSContext *ctx, JSPropertyEnum *tab,
                         uint32_t len);
int JS_GetOwnProperty(JSContext *ctx, JSPropertyDescriptor *desc,
                      JSValueConst obj, JSAtom prop);

JSValue JS_Call(JSContext *ctx, JSValueConst func_obj, JSValueConst this_obj,
                int argc, JSValueConst *argv);
JSValue JS_Invoke(JSContext *ctx, JSValueConst this_val, JSAtom atom,
                  int argc, JSValueConst *argv);
JSValue JS_CallConstructor(JSContext *ctx, JSValueConst func_obj,
                           int argc, JSValueConst *argv);
JSValue JS_CallConstructor2(JSContext *ctx, JSValueConst func_obj,
                            JSValueConst new_target,
                            int argc, JSValueConst *argv);
JS_BOOL JS_DetectModule(const char *input, size_t input_len);
/* 'input' must be zero terminated i.e. input[input_len] = '\0'. */
JSValue JS_Eval(JSContext *ctx, const char *input, size_t input_len,
                const char *filename, int eval_flags);
/* same as JS_Eval() but with an explicit 'this_obj' parameter */
JSValue JS_EvalThis(JSContext *ctx, JSValueConst this_obj,
                    const char *input, size_t input_len,
                    const char *filename, int eval_flags);
JSValue JS_GetGlobalObject(JSContext *ctx);
/* Servo: make the global object an instance of the (non-callable) class 'class_id': its
   finalizer, gc_mark, exotic methods and opaque (JS_SetOpaque) apply, and it keeps the
   semantics of a global object. Call it right after JS_NewContext(). Returns -1 if the
   class is not registered or callable. */
int JS_SetGlobalObjectClass(JSContext *ctx, JSClassID class_id);
/* Servo: 'this' in global code, in indirect eval and in sloppy-mode functions called with
   an undefined 'this', and the value of globalThis (the HTML WindowProxy). JS_UNDEFINED
   means the global object. JS_SetGlobalThis takes ownership of 'this_val'. */
void JS_SetGlobalThis(JSContext *ctx, JSValue this_val);
JSValue JS_GetGlobalThis(JSContext *ctx);
/* Servo: TRUE if the running function is strict mode code (for exotic methods receiving
   JS_PROP_THROW_STRICT) */
JS_BOOL JS_IsStrictMode(JSContext *ctx);
int JS_IsInstanceOf(JSContext *ctx, JSValueConst val, JSValueConst obj);
int JS_DefineProperty(JSContext *ctx, JSValueConst this_obj,
                      JSAtom prop, JSValueConst val,
                      JSValueConst getter, JSValueConst setter, int flags);
int JS_DefinePropertyValue(JSContext *ctx, JSValueConst this_obj,
                           JSAtom prop, JSValue val, int flags);
int JS_DefinePropertyValueUint32(JSContext *ctx, JSValueConst this_obj,
                                 uint32_t idx, JSValue val, int flags);
int JS_DefinePropertyValueStr(JSContext *ctx, JSValueConst this_obj,
                              const char *prop, JSValue val, int flags);
int JS_DefinePropertyGetSet(JSContext *ctx, JSValueConst this_obj,
                            JSAtom prop, JSValue getter, JSValue setter,
                            int flags);
void JS_SetOpaque(JSValue obj, void *opaque);
void *JS_GetOpaque(JSValueConst obj, JSClassID class_id);
void *JS_GetOpaque2(JSContext *ctx, JSValueConst obj, JSClassID class_id);
void *JS_GetAnyOpaque(JSValueConst obj, JSClassID *class_id);

/* 'buf' must be zero terminated i.e. buf[buf_len] = '\0'. */
JSValue JS_ParseJSON(JSContext *ctx, const char *buf, size_t buf_len,
                     const char *filename);
#define JS_PARSE_JSON_EXT (1 << 0) /* allow extended JSON */
JSValue JS_ParseJSON2(JSContext *ctx, const char *buf, size_t buf_len,
                      const char *filename, int flags);
JSValue JS_JSONStringify(JSContext *ctx, JSValueConst obj,
                         JSValueConst replacer, JSValueConst space0);

typedef void JSFreeArrayBufferDataFunc(JSRuntime *rt, void *opaque, void *ptr);
JSValue JS_NewArrayBuffer(JSContext *ctx, uint8_t *buf, size_t len,
                          JSFreeArrayBufferDataFunc *free_func, void *opaque,
                          JS_BOOL is_shared);
JSValue JS_NewArrayBufferCopy(JSContext *ctx, const uint8_t *buf, size_t len);
void JS_DetachArrayBuffer(JSContext *ctx, JSValueConst obj);
uint8_t *JS_GetArrayBuffer(JSContext *ctx, size_t *psize, JSValueConst obj);

typedef enum JSTypedArrayEnum {
    JS_TYPED_ARRAY_UINT8C = 0,
    JS_TYPED_ARRAY_INT8,
    JS_TYPED_ARRAY_UINT8,
    JS_TYPED_ARRAY_INT16,
    JS_TYPED_ARRAY_UINT16,
    JS_TYPED_ARRAY_INT32,
    JS_TYPED_ARRAY_UINT32,
    JS_TYPED_ARRAY_BIG_INT64,
    JS_TYPED_ARRAY_BIG_UINT64,
    JS_TYPED_ARRAY_FLOAT16,
    JS_TYPED_ARRAY_FLOAT32,
    JS_TYPED_ARRAY_FLOAT64,
} JSTypedArrayEnum;

JSValue JS_NewTypedArray(JSContext *ctx, int argc, JSValueConst *argv,
                         JSTypedArrayEnum array_type);
JSValue JS_GetTypedArrayBuffer(JSContext *ctx, JSValueConst obj,
                               size_t *pbyte_offset,
                               size_t *pbyte_length,
                               size_t *pbytes_per_element);

/* Servo: ArrayBuffer and view storage without exceptions or copies, stealing
   ArrayBuffer contents, and DataView creation. */
typedef enum JSBufferKind {
    JS_BUFFER_NONE,
    JS_BUFFER_ARRAY_BUFFER,
    JS_BUFFER_SHARED_ARRAY_BUFFER,
    JS_BUFFER_TYPED_ARRAY,
    JS_BUFFER_DATA_VIEW,
} JSBufferKind;

typedef struct JSBufferInfo {
    JSValueConst buffer; /* the (Shared)ArrayBuffer itself or the view's buffer; not duplicated */
    uint8_t *data;       /* the object's bytes; NULL if detached or out of bounds */
    size_t byte_offset;  /* offset of 'data' in 'buffer'; 0 if detached or out of bounds */
    size_t byte_length;  /* 0 if detached or out of bounds */
    int type;            /* JSTypedArrayEnum of a typed array, -1 otherwise */
    JS_BOOL detached;    /* 'buffer' is detached */
    JS_BOOL shared;      /* 'buffer' is a SharedArrayBuffer */
    JS_BOOL resizable;   /* 'buffer' is resizable or growable */
    JS_BOOL length_tracking; /* a view whose length follows its resizable buffer */
} JSBufferInfo;

/* Fills 'info' if 'obj' is a (Shared)ArrayBuffer, typed array or DataView. 'data' is valid
   until JS code runs. Never throws. */
JSBufferKind JS_GetBufferInfo(JSValueConst obj, JSBufferInfo *info);
/* Detaches a non-shared ArrayBuffer and returns its former contents (never NULL on success),
   to be freed with js_free_rt(). External contents are copied and released first. */
uint8_t *JS_StealArrayBuffer(JSContext *ctx, size_t *plen, JSValueConst obj);
/* An ArrayBuffer that takes ownership of 'buf', allocated with js_malloc_rt(). On failure,
   'buf' still belongs to the caller. */
JSValue JS_NewArrayBufferOwned(JSContext *ctx, uint8_t *buf, size_t len);
/* A 'byte_length' of SIZE_MAX makes a length-tracking view of a resizable buffer. */
JSValue JS_NewDataView(JSContext *ctx, JSValueConst buffer, size_t byte_offset,
                       size_t byte_length);
typedef struct {
    void *(*sab_alloc)(void *opaque, size_t size);
    void (*sab_free)(void *opaque, void *ptr);
    void (*sab_dup)(void *opaque, void *ptr);
    void *sab_opaque;
} JSSharedArrayBufferFunctions;
void JS_SetSharedArrayBufferFunctions(JSRuntime *rt,
                                      const JSSharedArrayBufferFunctions *sf);

typedef enum JSPromiseStateEnum {
    JS_PROMISE_PENDING,
    JS_PROMISE_FULFILLED,
    JS_PROMISE_REJECTED,
} JSPromiseStateEnum;

JSValue JS_NewPromiseCapability(JSContext *ctx, JSValue *resolving_funcs);
JSPromiseStateEnum JS_PromiseState(JSContext *ctx, JSValue promise);
JSValue JS_PromiseResult(JSContext *ctx, JSValue promise);

/* is_handled = TRUE means that the rejection is handled */
typedef void JSHostPromiseRejectionTracker(JSContext *ctx, JSValueConst promise,
                                           JSValueConst reason,
                                           JS_BOOL is_handled, void *opaque);
void JS_SetHostPromiseRejectionTracker(JSRuntime *rt, JSHostPromiseRejectionTracker *cb, void *opaque);
/* Servo: promise operations of the SpiderMonkey API. JS_PromiseResolve() is
   %Promise%.resolve() / %Promise%.reject(). JS_SettlePromise() resolves or rejects a
   pending promise (no-op otherwise). JS_PerformPromiseThen() adds reactions; without
   'create_result' it returns JS_UNDEFINED and the handlers' results are ignored. */
JSValue JS_PromiseResolve(JSContext *ctx, JSValueConst value, JS_BOOL is_reject);
int JS_SettlePromise(JSContext *ctx, JSValueConst promise, JSValueConst value,
                     JS_BOOL is_reject);
JSValue JS_PerformPromiseThen(JSContext *ctx, JSValueConst promise,
                              JSValueConst on_fulfilled, JSValueConst on_rejected,
                              JS_BOOL create_result);
JS_BOOL JS_GetPromiseIsHandled(JSValueConst promise);
void JS_SetPromiseIsHandled(JSValueConst promise, JS_BOOL is_handled);

/* return != 0 if the JS code needs to be interrupted */
typedef int JSInterruptHandler(JSRuntime *rt, void *opaque);
void JS_SetInterruptHandler(JSRuntime *rt, JSInterruptHandler *cb, void *opaque);
/* Servo: host check before eval() and the Function constructors compile code
   (HostEnsureCanCompileStrings). 'code' is the source text; for the Function
   constructors, from the parameter list to the closing brace, as SpiderMonkey passes it.
   For the Function constructors, 'arg_strings' (converted to strings) and 'args' (as
   passed) are the parameters followed by the body. For eval, 'args' is the argument as
   passed (argc 1): the code itself, or an object whose code the code-for-eval hook gave.
   Return 1 to allow, 0 to block (an EvalError is thrown) or < 0 on exception. */
typedef enum JSCompileKindEnum {
    JS_COMPILE_DIRECT_EVAL,
    JS_COMPILE_INDIRECT_EVAL,
    JS_COMPILE_FUNCTION,
} JSCompileKindEnum;
typedef int JSCanCompileStringsHook(JSContext *ctx, JSCompileKindEnum kind,
                                    JSValueConst code, int argc,
                                    JSValueConst *arg_strings, JSValueConst *args,
                                    void *opaque);
void JS_SetCanCompileStringsHook(JSRuntime *rt, JSCanCompileStringsHook *hook,
                                 void *opaque);
/* Servo: the code of an eval() argument that is an object (HostGetCodeForEval, e.g. a
   TrustedScript): a string to evaluate, JS_UNDEFINED to return the argument unchanged, or
   JS_EXCEPTION. */
typedef JSValue JSCodeForEvalHook(JSContext *ctx, JSValueConst obj, void *opaque);
void JS_SetCodeForEvalHook(JSRuntime *rt, JSCodeForEvalHook *hook, void *opaque);
/* Servo: whether an object that isn't an Error has [[ErrorData]] (Error.isError), e.g. a
   DOMException. Must not run JavaScript. */
typedef JS_BOOL JSIsErrorHook(JSContext *ctx, JSValueConst obj);
void JS_SetIsErrorHook(JSRuntime *rt, JSIsErrorHook *hook);
/* Servo: whether the cycle collector is freeing a garbage cycle. Its objects are finalized
   in any order, so finalizers must not reach the other objects of the cycle (SpiderMonkey's
   finalization during GC). */
JS_BOOL JS_IsCollectingCycles(JSRuntime *rt);
/* Servo: whether the native stack is too deep for more recursion; if so, throws an
   InternalError ("stack overflow") and returns TRUE. For the host's own recursion. */
JS_BOOL JS_CheckStackOverflow(JSContext *ctx);
/* Servo: the realm 'obj' was created in, where known (functions and proxies); NULL otherwise */
JSContext *JS_GetObjectRealm(JSValueConst obj);
/* Servo: debug hook called when an object is freed, before its finalizer */
typedef void JSFreeObjectHook(JSRuntime *rt, JSValueConst obj);
void JS_SetFreeObjectHook(JSRuntime *rt, JSFreeObjectHook *hook);
/* if can_block is TRUE, Atomics.wait() can be used */
void JS_SetCanBlock(JSRuntime *rt, JS_BOOL can_block);
/* select which debug info is stripped from the compiled code */
#define JS_STRIP_SOURCE (1 << 0) /* strip source code */
#define JS_STRIP_DEBUG  (1 << 1) /* strip all debug info including source code */
void JS_SetStripInfo(JSRuntime *rt, int flags);
int JS_GetStripInfo(JSRuntime *rt);

/* set the [IsHTMLDDA] internal slot */
void JS_SetIsHTMLDDA(JSContext *ctx, JSValueConst obj);

typedef struct JSModuleDef JSModuleDef;

/* return the module specifier (allocated with js_malloc()) or NULL if
   exception */
typedef char *JSModuleNormalizeFunc(JSContext *ctx,
                                    const char *module_base_name,
                                    const char *module_name, void *opaque);
typedef JSModuleDef *JSModuleLoaderFunc(JSContext *ctx,
                                        const char *module_name, void *opaque);
typedef JSModuleDef *JSModuleLoaderFunc2(JSContext *ctx,
                                         const char *module_name, void *opaque,
                                         JSValueConst attributes);
/* return -1 if exception, 0 if OK */
typedef int JSModuleCheckSupportedImportAttributes(JSContext *ctx, void *opaque,
                                                   JSValueConst attributes);
                                                   
/* module_normalize = NULL is allowed and invokes the default module
   filename normalizer */
void JS_SetModuleLoaderFunc(JSRuntime *rt,
                            JSModuleNormalizeFunc *module_normalize,
                            JSModuleLoaderFunc *module_loader, void *opaque);
/* same as JS_SetModuleLoaderFunc but with attributes. if
   module_check_attrs = NULL, no attribute checking is done. */
void JS_SetModuleLoaderFunc2(JSRuntime *rt,
                             JSModuleNormalizeFunc *module_normalize,
                             JSModuleLoaderFunc2 *module_loader,
                             JSModuleCheckSupportedImportAttributes *module_check_attrs,
                             void *opaque);
/* return the import.meta object of a module */
JSValue JS_GetImportMeta(JSContext *ctx, JSModuleDef *m);
JSAtom JS_GetModuleName(JSContext *ctx, JSModuleDef *m);
JSValue JS_GetModuleNamespace(JSContext *ctx, JSModuleDef *m);
/* Servo: host-driven module loading. The requests of a module (in source order,
   not deduplicated) are its specifiers and their import attributes (JS_UNDEFINED
   or an object). JS_LinkModule() calls 'resolve' for each request of the module
   graph that has no module yet, then links the graph like the Link() method of
   module records. 'resolve' returns NULL with an exception on failure; the
   returned module must stay alive for as long as 'm' (e.g. in the same context). */
int JS_GetModuleRequestCount(JSModuleDef *m);
JSAtom JS_GetModuleRequestSpecifier(JSContext *ctx, JSModuleDef *m, int idx);
JSValue JS_GetModuleRequestAttributes(JSContext *ctx, JSModuleDef *m, int idx);
typedef JSModuleDef *JSModuleResolveFunc(JSContext *ctx, JSModuleDef *m, int idx,
                                         void *opaque);
int JS_LinkModule(JSContext *ctx, JSModuleDef *m, JSModuleResolveFunc *resolve,
                  void *opaque);
JS_BOOL JS_IsModuleLinked(JSModuleDef *m);
/* Servo: whether 'm' was created with JS_NewCModule() */
JS_BOOL JS_IsCModule(JSModuleDef *m);
/* Servo: the first module of 'ctx' named 'name' (the file name of its code), like
   import.meta and import() find the calling module, or NULL */
JSModuleDef *JS_FindLoadedModule(JSContext *ctx, JSAtom name);
/* Servo: the file name of a compiled script (JS_TAG_FUNCTION_BYTECODE), or
   JS_ATOM_NULL */
JSAtom JS_GetScriptFileName(JSContext *ctx, JSValueConst script);
/* Servo (debugging): the number of live objects of each class id below 'len' (other
   counts are not computed), and the runtime's number of class ids. */
uint32_t JS_CountObjectsByClass(JSRuntime *rt, uint32_t *counts, uint32_t len);
/* Servo (debugging): called during each GC, before anything is freed, for every GC thing
   that is referenced from outside the GC heap ('ref_count' external references): the roots
   of the collection. 'gc_obj_type' is 0 for a JSObject ('ptr'), 5 for a JSContext. A last
   call with 'gc_obj_type' -1 ends the list. */
typedef void JSGCRootsHook(JSRuntime *rt, int gc_obj_type, void *ptr, int ref_count, void *opaque);
void JS_SetGCRootsHook(JSRuntime *rt, JSGCRootsHook *hook, void *opaque);
/* Servo (debugging): calls 'visit' for every GC thing that the GC thing 'ptr' references.
   Only valid in a JSGCRootsHook. */
typedef void JSGCChildVisitor(JSRuntime *rt, int gc_obj_type, void *ptr, void *opaque);
void JS_VisitGCChildren(JSRuntime *rt, void *ptr, JSGCChildVisitor *visit, void *opaque);
/* Servo: the prototype in an object's shape, without a reference, or NULL. */
void *JS_GetObjectShapeProto(void *obj);
/* Servo: JS_HasProperty() with a cache for lookups that start at a prototype and only
   meet ordinary objects (no exotic behavior). With JS_HAS_CACHED_ORDINARY_ONLY, other
   lookups return -2 without looking anything up; with JS_HAS_CACHED_NO_INDEX, so do array
   indices found on the chain. */
#define JS_HAS_CACHED_ORDINARY_ONLY (1 << 0)
#define JS_HAS_CACHED_NO_INDEX      (1 << 1)
int JS_HasPropertyCached(JSContext *ctx, JSValueConst obj, JSAtom prop, int flags);
/* Servo (debugging): a class's name, formatted into 'buf' if needed. */
const char *JS_GetClassNameRT(JSRuntime *rt, JSClassID class_id, char *buf, int buf_size);
/* Servo: the host-defined value of a compiled script (JS_TAG_FUNCTION_BYTECODE),
   shared with the eval and Function code it creates. The setter takes ownership
   of 'val' and returns < 0 for modules and non-scripts. The finalizer is called
   for a replaced value and when the last code of the script is freed, and also for
   the private value of a JavaScript module (JS_SetModulePrivateValue; not of a C
   module) when it is replaced and when the module is freed. */
typedef void JSScriptPrivateFinalizer(JSRuntime *rt, JSValueConst val);
void JS_SetScriptPrivateFinalizer(JSRuntime *rt, JSScriptPrivateFinalizer *finalizer);
int JS_SetScriptPrivateValue(JSContext *ctx, JSValueConst script, JSValue val);
JSValue JS_GetScriptPrivateValue(JSContext *ctx, JSValueConst script);
/* Servo: scripts as the host's referrers. JS_GetActiveScript() returns the code
   (JS_TAG_FUNCTION_BYTECODE) of the innermost scripted frame, or JS_UNDEFINED;
   JS_GetModuleScript() the code of a JavaScript module, or JS_UNDEFINED;
   JS_GetScriptModule() the module whose code 'script' is part of, or NULL. */
JSValue JS_GetActiveScript(JSContext *ctx);
JSValue JS_GetModuleScript(JSContext *ctx, JSModuleDef *m);
JSModuleDef *JS_GetScriptModule(JSValueConst script);
/* Servo: the realm whose global a script's code runs in (the realm it was
   compiled in), or NULL if 'script' is not code. */
JSContext *JS_GetScriptRealm(JSValueConst script);
/* Servo: host hooks. The import.meta hook populates the new import.meta object of
   'm'. The dynamic import hook replaces the module loader for import(): 'm' is
   the module of the calling code, or NULL for script code, whose host-defined
   value is 'script_private' (JS_UNDEFINED if none); the host settles 'promise',
   the result of import(). Both return < 0 on exception. */
typedef int JSImportMetaHook(JSContext *ctx, JSModuleDef *m, JSValueConst meta,
                             void *opaque);
typedef int JSDynamicImportHook(JSContext *ctx, JSModuleDef *m, JSValueConst script_private,
                                JSValueConst specifier, JSValueConst attributes,
                                JSValueConst promise, void *opaque);
void JS_SetImportMetaHook(JSRuntime *rt, JSImportMetaHook *hook, void *opaque);
void JS_SetDynamicImportHook(JSRuntime *rt, JSDynamicImportHook *hook, void *opaque);
/* Servo: marks a settled promise as handled (SetSettledPromiseIsHandled), telling
   the rejection tracker as adding a reaction would */
void JS_MarkPromiseHandled(JSContext *ctx, JSValueConst promise);

/* JS Job support */

typedef JSValue JSJobFunc(JSContext *ctx, int argc, JSValueConst *argv);
int JS_EnqueueJob(JSContext *ctx, JSJobFunc *job_func, int argc, JSValueConst *argv);
/* Servo: if set, the jobs of JS_EnqueueJob() (promise jobs) are passed to the hook
   instead of the internal job list. 'argv' is borrowed. 'host_defined' is what the
   HostMakeJobCallback hook returned when the reaction was created, JS_UNDEFINED for
   other jobs. Return < 0 on exception. */
typedef int JSEnqueueJobHook(JSContext *ctx, JSJobFunc *job_func, int argc,
                             JSValueConst *argv, JSValueConst promise,
                             JSValueConst host_defined, void *opaque);
void JS_SetEnqueueJobHook(JSRuntime *rt, JSEnqueueJobHook *hook, void *opaque);
/* Servo: HostMakeJobCallback. Called when then() or await creates a promise
   reaction; the reaction keeps the result (a new reference, JS_UNDEFINED for none)
   and passes it to the enqueue hook with its job (Q32). */
typedef JSValue JSHostMakeJobCallbackHook(JSContext *ctx, void *opaque);
void JS_SetHostMakeJobCallbackHook(JSRuntime *rt, JSHostMakeJobCallbackHook *hook,
                                   void *opaque);
/* Servo: a promise's user input state for the host (SpiderMonkey's
   PromiseUserInputEventHandlingState, 0 when not set). then() copies it to the
   promise it creates, and the enqueue hook receives that promise for reaction
   jobs, the promise to resolve for thenable jobs, else JS_UNDEFINED (Q30). */
int JS_GetPromiseUserInputState(JSValueConst promise);
void JS_SetPromiseUserInputState(JSValueConst promise, int state);

JS_BOOL JS_IsJobPending(JSRuntime *rt);
int JS_ExecutePendingJob(JSRuntime *rt, JSContext **pctx);

/* Servo: internal slots for HTML structured serialization, without running JS code. */
/* [[NumberData]], [[StringData]], [[BooleanData]], [[BigIntData]] or [[DateValue]] of
   'obj'; JS_UNDEFINED for other objects */
JSValue JS_GetPrimitiveObjectData(JSContext *ctx, JSValueConst obj);
/* a Number, String, Boolean or BigInt wrapper object of the current realm */
JSValue JS_NewPrimitiveObject(JSContext *ctx, JSValueConst val);
/* [[OriginalSource]] of a RegExp; '*pflags' receives its flags as a string */
JSValue JS_GetRegExpOriginal(JSContext *ctx, JSValueConst obj, JSValue *pflags);
JSValue JS_NewRegExpOriginal(JSContext *ctx, JSValueConst source, JSValueConst flags);
JSValue JS_NewMapOrSet(JSContext *ctx, JS_BOOL is_set);
/* a new array with a snapshot of the entries of a Map ([key0, value0, key1, ...]) or a
   Set ([key0, key1, ...]) */
JSValue JS_GetMapRecords(JSContext *ctx, JSValueConst obj);
/* what Map.prototype.set / Set.prototype.add do, without looking them up ('value' is
   ignored for Sets) */
int JS_AddMapRecord(JSContext *ctx, JSValueConst obj, JSValueConst key,
                    JSValueConst value);
/* an Error object without stack, with the prototype of %Error% (error_num < 0) or of the
   native error 'error_num' (EvalError, RangeError, ReferenceError, SyntaxError, TypeError,
   URIError, InternalError, AggregateError) */
JSValue JS_NewNativeErrorObject(JSContext *ctx, int error_num);
/* the two's complement 32-bit words of a BigInt, least significant first: returns their
   count and stores at most 'max_words' of them */
size_t JS_GetBigIntWords(JSValueConst val, uint32_t *words, size_t max_words);
JSValue JS_NewBigIntWords(JSContext *ctx, const uint32_t *words, size_t count);
/* [[ArrayBufferMaxByteLength]], or -1 if 'obj' is not a resizable ArrayBuffer */
int64_t JS_GetArrayBufferMaxByteLength(JSValueConst obj);
JSValue JS_NewResizableArrayBufferCopy(JSContext *ctx, const uint8_t *buf, size_t len,
                                       size_t max_len);

/* Object Writer/Reader (currently only used to handle precompiled code) */
#define JS_WRITE_OBJ_BYTECODE  (1 << 0) /* allow function/module */
#define JS_WRITE_OBJ_BSWAP     (1 << 1) /* byte swapped output */
#define JS_WRITE_OBJ_SAB       (1 << 2) /* allow SharedArrayBuffer */
#define JS_WRITE_OBJ_REFERENCE (1 << 3) /* allow object references to
                                           encode arbitrary object
                                           graph */
uint8_t *JS_WriteObject(JSContext *ctx, size_t *psize, JSValueConst obj,
                        int flags);
uint8_t *JS_WriteObject2(JSContext *ctx, size_t *psize, JSValueConst obj,
                         int flags, uint8_t ***psab_tab, size_t *psab_tab_len);

#define JS_READ_OBJ_BYTECODE  (1 << 0) /* allow function/module */
#define JS_READ_OBJ_ROM_DATA  (1 << 1) /* avoid duplicating 'buf' data */
#define JS_READ_OBJ_SAB       (1 << 2) /* allow SharedArrayBuffer */
#define JS_READ_OBJ_REFERENCE (1 << 3) /* allow object references */
JSValue JS_ReadObject(JSContext *ctx, const uint8_t *buf, size_t buf_len,
                      int flags);
/* instantiate and evaluate a bytecode function. Only used when
   reading a script or module with JS_ReadObject() */
JSValue JS_EvalFunction(JSContext *ctx, JSValue fun_obj);
/* load the dependencies of the module 'obj'. Useful when JS_ReadObject()
   returns a module. */
int JS_ResolveModule(JSContext *ctx, JSValueConst obj);

/* only exported for os.Worker() */
JSAtom JS_GetScriptOrModuleName(JSContext *ctx, int n_stack_levels);
/* only exported for os.Worker() */
JSValue JS_LoadModule(JSContext *ctx, const char *basename,
                      const char *filename);

/* C function definition */
typedef enum JSCFunctionEnum {  /* XXX: should rename for namespace isolation */
    JS_CFUNC_generic,
    JS_CFUNC_generic_magic,
    JS_CFUNC_constructor,
    JS_CFUNC_constructor_magic,
    JS_CFUNC_constructor_or_func,
    JS_CFUNC_constructor_or_func_magic,
    JS_CFUNC_f_f,
    JS_CFUNC_f_f_f,
    JS_CFUNC_getter,
    JS_CFUNC_setter,
    JS_CFUNC_getter_magic,
    JS_CFUNC_setter_magic,
    JS_CFUNC_iterator_next,
} JSCFunctionEnum;

typedef union JSCFunctionType {
    JSCFunction *generic;
    JSValue (*generic_magic)(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv, int magic);
    JSCFunction *constructor;
    JSValue (*constructor_magic)(JSContext *ctx, JSValueConst new_target, int argc, JSValueConst *argv, int magic);
    JSCFunction *constructor_or_func;
    double (*f_f)(double);
    double (*f_f_f)(double, double);
    JSValue (*getter)(JSContext *ctx, JSValueConst this_val);
    JSValue (*setter)(JSContext *ctx, JSValueConst this_val, JSValueConst val);
    JSValue (*getter_magic)(JSContext *ctx, JSValueConst this_val, int magic);
    JSValue (*setter_magic)(JSContext *ctx, JSValueConst this_val, JSValueConst val, int magic);
    JSValue (*iterator_next)(JSContext *ctx, JSValueConst this_val,
                             int argc, JSValueConst *argv, int *pdone, int magic);
} JSCFunctionType;

JSValue JS_NewCFunction2(JSContext *ctx, JSCFunction *func,
                         const char *name,
                         int length, JSCFunctionEnum cproto, int magic);
JSValue JS_NewCFunctionData(JSContext *ctx, JSCFunctionData *func,
                            int length, int magic, int data_len,
                            JSValueConst *data);

static inline JSValue JS_NewCFunction(JSContext *ctx, JSCFunction *func, const char *name,
                                      int length)
{
    return JS_NewCFunction2(ctx, func, name, length, JS_CFUNC_generic, 0);
}

static inline JSValue JS_NewCFunctionMagic(JSContext *ctx, JSCFunctionMagic *func,
                                           const char *name,
                                           int length, JSCFunctionEnum cproto, int magic)
{
    /* Used to squelch a -Wcast-function-type warning. */
    JSCFunctionType ft = { .generic_magic = func };
    return JS_NewCFunction2(ctx, ft.generic, name, length, cproto, magic);
}
int JS_SetConstructor(JSContext *ctx, JSValueConst func_obj,
                      JSValueConst proto);

/* C property definition */

typedef struct JSCFunctionListEntry {
    const char *name;
    uint8_t prop_flags;
    uint8_t def_type;
    int16_t magic;
    union {
        struct {
            uint8_t length; /* XXX: should move outside union */
            uint8_t cproto; /* XXX: should move outside union */
            JSCFunctionType cfunc;
        } func;
        struct {
            JSCFunctionType get;
            JSCFunctionType set;
        } getset;
        struct {
            const char *name;
            int base;
        } alias;
        struct {
            const struct JSCFunctionListEntry *tab;
            int len;
        } prop_list;
        const char *str;
        int32_t i32;
        int64_t i64;
        double f64;
    } u;
} JSCFunctionListEntry;

#define JS_DEF_CFUNC          0
#define JS_DEF_CGETSET        1
#define JS_DEF_CGETSET_MAGIC  2
#define JS_DEF_PROP_STRING    3
#define JS_DEF_PROP_INT32     4
#define JS_DEF_PROP_INT64     5
#define JS_DEF_PROP_DOUBLE    6
#define JS_DEF_PROP_UNDEFINED 7
#define JS_DEF_OBJECT         8
#define JS_DEF_ALIAS          9
#define JS_DEF_PROP_ATOM     10
#define JS_DEF_PROP_BOOL     11

/* Note: c++ does not like nested designators */
#define JS_CFUNC_DEF(name, length, func1) { name, JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE, JS_DEF_CFUNC, 0, .u = { .func = { length, JS_CFUNC_generic, { .generic = func1 } } } }
#define JS_CFUNC_MAGIC_DEF(name, length, func1, magic) { name, JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE, JS_DEF_CFUNC, magic, .u = { .func = { length, JS_CFUNC_generic_magic, { .generic_magic = func1 } } } }
#define JS_CFUNC_SPECIAL_DEF(name, length, cproto, func1) { name, JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE, JS_DEF_CFUNC, 0, .u = { .func = { length, JS_CFUNC_ ## cproto, { .cproto = func1 } } } }
#define JS_ITERATOR_NEXT_DEF(name, length, func1, magic) { name, JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE, JS_DEF_CFUNC, magic, .u = { .func = { length, JS_CFUNC_iterator_next, { .iterator_next = func1 } } } }
#define JS_CGETSET_DEF(name, fgetter, fsetter) { name, JS_PROP_CONFIGURABLE, JS_DEF_CGETSET, 0, .u = { .getset = { .get = { .getter = fgetter }, .set = { .setter = fsetter } } } }
#define JS_CGETSET_MAGIC_DEF(name, fgetter, fsetter, magic) { name, JS_PROP_CONFIGURABLE, JS_DEF_CGETSET_MAGIC, magic, .u = { .getset = { .get = { .getter_magic = fgetter }, .set = { .setter_magic = fsetter } } } }
#define JS_PROP_STRING_DEF(name, cstr, prop_flags) { name, prop_flags, JS_DEF_PROP_STRING, 0, .u = { .str = cstr } }
#define JS_PROP_INT32_DEF(name, val, prop_flags) { name, prop_flags, JS_DEF_PROP_INT32, 0, .u = { .i32 = val } }
#define JS_PROP_INT64_DEF(name, val, prop_flags) { name, prop_flags, JS_DEF_PROP_INT64, 0, .u = { .i64 = val } }
#define JS_PROP_DOUBLE_DEF(name, val, prop_flags) { name, prop_flags, JS_DEF_PROP_DOUBLE, 0, .u = { .f64 = val } }
#define JS_PROP_UNDEFINED_DEF(name, prop_flags) { name, prop_flags, JS_DEF_PROP_UNDEFINED, 0, .u = { .i32 = 0 } }
#define JS_PROP_ATOM_DEF(name, val, prop_flags) { name, prop_flags, JS_DEF_PROP_ATOM, 0, .u = { .i32 = val } }
#define JS_PROP_BOOL_DEF(name, val, prop_flags) { name, prop_flags, JS_DEF_PROP_BOOL, 0, .u = { .i32 = val } }
#define JS_OBJECT_DEF(name, tab, len, prop_flags) { name, prop_flags, JS_DEF_OBJECT, 0, .u = { .prop_list = { tab, len } } }
#define JS_ALIAS_DEF(name, from) { name, JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE, JS_DEF_ALIAS, 0, .u = { .alias = { from, -1 } } }
#define JS_ALIAS_BASE_DEF(name, from, base) { name, JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE, JS_DEF_ALIAS, 0, .u = { .alias = { from, base } } }

int JS_SetPropertyFunctionList(JSContext *ctx, JSValueConst obj,
                               const JSCFunctionListEntry *tab,
                               int len);

/* C module definition */

typedef int JSModuleInitFunc(JSContext *ctx, JSModuleDef *m);

JSModuleDef *JS_NewCModule(JSContext *ctx, const char *name_str,
                           JSModuleInitFunc *func);
/* can only be called before the module is instantiated */
int JS_AddModuleExport(JSContext *ctx, JSModuleDef *m, const char *name_str);
int JS_AddModuleExportList(JSContext *ctx, JSModuleDef *m,
                           const JSCFunctionListEntry *tab, int len);
/* can only be called after the module is instantiated */
int JS_SetModuleExport(JSContext *ctx, JSModuleDef *m, const char *export_name,
                       JSValue val);
int JS_SetModuleExportList(JSContext *ctx, JSModuleDef *m,
                           const JSCFunctionListEntry *tab, int len);
/* associate a JSValue to a C module */
int JS_SetModulePrivateValue(JSContext *ctx, JSModuleDef *m, JSValue val);
JSValue JS_GetModulePrivateValue(JSContext *ctx, JSModuleDef *m);
                        
/* debug value output */

typedef struct {
    JS_BOOL show_hidden : 8; /* only show enumerable properties */
    JS_BOOL raw_dump : 8; /* avoid doing autoinit and avoid any malloc() call (for internal use) */
    uint32_t max_depth; /* recurse up to this depth, 0 = no limit */
    uint32_t max_string_length; /* print no more than this length for
                                   strings, 0 = no limit */
    uint32_t max_item_count; /*  print no more than this count for
                                 arrays or objects, 0 = no limit */
} JSPrintValueOptions;

typedef void JSPrintValueWrite(void *opaque, const char *buf, size_t len);

void JS_PrintValueSetDefaultOptions(JSPrintValueOptions *options);
void JS_PrintValueRT(JSRuntime *rt, JSPrintValueWrite *write_func, void *write_opaque,
                     JSValueConst val, const JSPrintValueOptions *options);
void JS_PrintValue(JSContext *ctx, JSPrintValueWrite *write_func, void *write_opaque,
                   JSValueConst val, const JSPrintValueOptions *options);

#undef js_unlikely
#undef js_force_inline

#ifdef __cplusplus
} /* extern "C" { */
#endif

#endif /* QUICKJS_H */
