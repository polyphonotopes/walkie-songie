import { detectPitch, initSwiftF0 } from './snippets/walkie-songie-54ae4c155955ba2c/assets/onnx-bridge.js';
import * as __wbg_star0 from './snippets/walkie-songie-54ae4c155955ba2c/assets/onnx-bridge.js';

const lAudioContext = (typeof AudioContext !== 'undefined' ? AudioContext : (typeof webkitAudioContext !== 'undefined' ? webkitAudioContext : undefined));
let wasm;

function addHeapObject(obj) {
    if (heap_next === heap.length) heap.push(heap.length + 1);
    const idx = heap_next;
    heap_next = heap[idx];

    heap[idx] = obj;
    return idx;
}

function addBorrowedObject(obj) {
    if (stack_pointer == 1) throw new Error('out of js stack');
    heap[--stack_pointer] = obj;
    return stack_pointer;
}

const CLOSURE_DTORS = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(state => state.dtor(state.a, state.b));

function debugString(val) {
    // primitive types
    const type = typeof val;
    if (type == 'number' || type == 'boolean' || val == null) {
        return  `${val}`;
    }
    if (type == 'string') {
        return `"${val}"`;
    }
    if (type == 'symbol') {
        const description = val.description;
        if (description == null) {
            return 'Symbol';
        } else {
            return `Symbol(${description})`;
        }
    }
    if (type == 'function') {
        const name = val.name;
        if (typeof name == 'string' && name.length > 0) {
            return `Function(${name})`;
        } else {
            return 'Function';
        }
    }
    // objects
    if (Array.isArray(val)) {
        const length = val.length;
        let debug = '[';
        if (length > 0) {
            debug += debugString(val[0]);
        }
        for(let i = 1; i < length; i++) {
            debug += ', ' + debugString(val[i]);
        }
        debug += ']';
        return debug;
    }
    // Test for built-in
    const builtInMatches = /\[object ([^\]]+)\]/.exec(toString.call(val));
    let className;
    if (builtInMatches && builtInMatches.length > 1) {
        className = builtInMatches[1];
    } else {
        // Failed to match the standard '[object ClassName]'
        return toString.call(val);
    }
    if (className == 'Object') {
        // we're a user defined class or Object
        // JSON.stringify avoids problems with cycles, and is generally much
        // easier than looping through ownProperties of `val`.
        try {
            return 'Object(' + JSON.stringify(val) + ')';
        } catch (_) {
            return 'Object';
        }
    }
    // errors
    if (val instanceof Error) {
        return `${val.name}: ${val.message}\n${val.stack}`;
    }
    // TODO we could test for more things here, like `Set`s and `Map`s.
    return className;
}

function dropObject(idx) {
    if (idx < 132) return;
    heap[idx] = heap_next;
    heap_next = idx;
}

function getArrayF32FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getFloat32ArrayMemory0().subarray(ptr / 4, ptr / 4 + len);
}

function getArrayU8FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
}

function getCachedStringFromWasm0(ptr, len) {
    if (ptr === 0) {
        return getObject(len);
    } else {
        return getStringFromWasm0(ptr, len);
    }
}

let cachedDataViewMemory0 = null;
function getDataViewMemory0() {
    if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer.detached === true || (cachedDataViewMemory0.buffer.detached === undefined && cachedDataViewMemory0.buffer !== wasm.memory.buffer)) {
        cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
    }
    return cachedDataViewMemory0;
}

let cachedFloat32ArrayMemory0 = null;
function getFloat32ArrayMemory0() {
    if (cachedFloat32ArrayMemory0 === null || cachedFloat32ArrayMemory0.byteLength === 0) {
        cachedFloat32ArrayMemory0 = new Float32Array(wasm.memory.buffer);
    }
    return cachedFloat32ArrayMemory0;
}

function getStringFromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return decodeText(ptr, len);
}

let cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
    if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
        cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
    }
    return cachedUint8ArrayMemory0;
}

function getObject(idx) { return heap[idx]; }

function handleError(f, args) {
    try {
        return f.apply(this, args);
    } catch (e) {
        wasm.__wbindgen_export3(addHeapObject(e));
    }
}

let heap = new Array(128).fill(undefined);
heap.push(undefined, null, true, false);

let heap_next = heap.length;

function isLikeNone(x) {
    return x === undefined || x === null;
}

function makeClosure(arg0, arg1, dtor, f) {
    const state = { a: arg0, b: arg1, cnt: 1, dtor };
    const real = (...args) => {

        // First up with a closure we increment the internal reference
        // count. This ensures that the Rust closure environment won't
        // be deallocated while we're invoking it.
        state.cnt++;
        try {
            return f(state.a, state.b, ...args);
        } finally {
            real._wbg_cb_unref();
        }
    };
    real._wbg_cb_unref = () => {
        if (--state.cnt === 0) {
            state.dtor(state.a, state.b);
            state.a = 0;
            CLOSURE_DTORS.unregister(state);
        }
    };
    CLOSURE_DTORS.register(real, state, state);
    return real;
}

function makeMutClosure(arg0, arg1, dtor, f) {
    const state = { a: arg0, b: arg1, cnt: 1, dtor };
    const real = (...args) => {

        // First up with a closure we increment the internal reference
        // count. This ensures that the Rust closure environment won't
        // be deallocated while we're invoking it.
        state.cnt++;
        const a = state.a;
        state.a = 0;
        try {
            return f(a, state.b, ...args);
        } finally {
            state.a = a;
            real._wbg_cb_unref();
        }
    };
    real._wbg_cb_unref = () => {
        if (--state.cnt === 0) {
            state.dtor(state.a, state.b);
            state.a = 0;
            CLOSURE_DTORS.unregister(state);
        }
    };
    CLOSURE_DTORS.register(real, state, state);
    return real;
}

function passArray8ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 1, 1) >>> 0;
    getUint8ArrayMemory0().set(arg, ptr / 1);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passArrayF32ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 4, 4) >>> 0;
    getFloat32ArrayMemory0().set(arg, ptr / 4);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passStringToWasm0(arg, malloc, realloc) {
    if (realloc === undefined) {
        const buf = cachedTextEncoder.encode(arg);
        const ptr = malloc(buf.length, 1) >>> 0;
        getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
        WASM_VECTOR_LEN = buf.length;
        return ptr;
    }

    let len = arg.length;
    let ptr = malloc(len, 1) >>> 0;

    const mem = getUint8ArrayMemory0();

    let offset = 0;

    for (; offset < len; offset++) {
        const code = arg.charCodeAt(offset);
        if (code > 0x7F) break;
        mem[ptr + offset] = code;
    }
    if (offset !== len) {
        if (offset !== 0) {
            arg = arg.slice(offset);
        }
        ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
        const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
        const ret = cachedTextEncoder.encodeInto(arg, view);

        offset += ret.written;
        ptr = realloc(ptr, len, offset, 1) >>> 0;
    }

    WASM_VECTOR_LEN = offset;
    return ptr;
}

let stack_pointer = 128;

function takeObject(idx) {
    const ret = getObject(idx);
    dropObject(idx);
    return ret;
}

let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
cachedTextDecoder.decode();
const MAX_SAFARI_DECODE_BYTES = 2146435072;
let numBytesDecoded = 0;
function decodeText(ptr, len) {
    numBytesDecoded += len;
    if (numBytesDecoded >= MAX_SAFARI_DECODE_BYTES) {
        cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
        cachedTextDecoder.decode();
        numBytesDecoded = len;
    }
    return cachedTextDecoder.decode(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}

const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        const buf = cachedTextEncoder.encode(arg);
        view.set(buf);
        return {
            read: arg.length,
            written: buf.length
        };
    }
}

let WASM_VECTOR_LEN = 0;

function __wasm_bindgen_func_elem_5003(arg0, arg1, arg2) {
    wasm.__wasm_bindgen_func_elem_5003(arg0, arg1, addHeapObject(arg2));
}

function __wasm_bindgen_func_elem_1795(arg0, arg1, arg2) {
    wasm.__wasm_bindgen_func_elem_1795(arg0, arg1, addHeapObject(arg2));
}

function __wasm_bindgen_func_elem_1793(arg0, arg1) {
    wasm.__wasm_bindgen_func_elem_1793(arg0, arg1);
}

function __wasm_bindgen_func_elem_1794(arg0, arg1, arg2) {
    wasm.__wasm_bindgen_func_elem_1794(arg0, arg1, addHeapObject(arg2));
}

function __wasm_bindgen_func_elem_4380(arg0, arg1, arg2) {
    wasm.__wasm_bindgen_func_elem_4380(arg0, arg1, addHeapObject(arg2));
}

function __wasm_bindgen_func_elem_5482(arg0, arg1) {
    wasm.__wasm_bindgen_func_elem_5482(arg0, arg1);
}

function __wasm_bindgen_func_elem_8784(arg0, arg1) {
    wasm.__wasm_bindgen_func_elem_8784(arg0, arg1);
}

function __wasm_bindgen_func_elem_10019(arg0, arg1, arg2) {
    wasm.__wasm_bindgen_func_elem_10019(arg0, arg1, addHeapObject(arg2));
}

function __wasm_bindgen_func_elem_9894(arg0, arg1, arg2) {
    try {
        wasm.__wasm_bindgen_func_elem_9894(arg0, arg1, addBorrowedObject(arg2));
    } finally {
        heap[stack_pointer++] = undefined;
    }
}

function __wasm_bindgen_func_elem_13511(arg0, arg1, arg2, arg3) {
    wasm.__wasm_bindgen_func_elem_13511(arg0, arg1, addHeapObject(arg2), addHeapObject(arg3));
}

const __wbindgen_enum_AudioContextState = ["suspended", "running", "closed"];

const __wbindgen_enum_BinaryType = ["blob", "arraybuffer"];

const __wbindgen_enum_IdbTransactionMode = ["readonly", "readwrite", "versionchange", "readwriteflush", "cleanup"];

const __wbindgen_enum_RtcDataChannelState = ["connecting", "open", "closing", "closed"];

const __wbindgen_enum_RtcDataChannelType = ["arraybuffer", "blob"];

const __wbindgen_enum_RtcIceConnectionState = ["new", "checking", "connected", "completed", "failed", "disconnected", "closed"];

const __wbindgen_enum_RtcIceGatheringState = ["new", "gathering", "complete"];

const __wbindgen_enum_RtcPeerConnectionState = ["closed", "failed", "disconnected", "new", "connecting", "connected"];

const __wbindgen_enum_RtcSdpType = ["offer", "pranswer", "answer", "rollback"];

const __wbindgen_enum_RtcSignalingState = ["stable", "have-local-offer", "have-remote-offer", "have-local-pranswer", "have-remote-pranswer", "closed"];

/**
 * Run the web application.
 */
export function run_app() {
    wasm.run_app();
}

const EXPECTED_RESPONSE_TYPES = new Set(['basic', 'cors', 'default']);

async function __wbg_load(module, imports) {
    if (typeof Response === 'function' && module instanceof Response) {
        if (typeof WebAssembly.instantiateStreaming === 'function') {
            try {
                return await WebAssembly.instantiateStreaming(module, imports);
            } catch (e) {
                const validResponse = module.ok && EXPECTED_RESPONSE_TYPES.has(module.type);

                if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                    console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                } else {
                    throw e;
                }
            }
        }

        const bytes = await module.arrayBuffer();
        return await WebAssembly.instantiate(bytes, imports);
    } else {
        const instance = await WebAssembly.instantiate(module, imports);

        if (instance instanceof WebAssembly.Instance) {
            return { instance, module };
        } else {
            return instance;
        }
    }
}

function __wbg_get_imports() {
    const imports = {};
    imports.wbg = {};
    imports.wbg.__wbg_WorkerGlobalScope_68dbbc2404209578 = function(arg0) {
        const ret = getObject(arg0).WorkerGlobalScope;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg___wbindgen_debug_string_adfb662ae34724b6 = function(arg0, arg1) {
        const ret = debugString(getObject(arg1));
        const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    };
    imports.wbg.__wbg___wbindgen_is_function_8d400b8b1af978cd = function(arg0) {
        const ret = typeof(getObject(arg0)) === 'function';
        return ret;
    };
    imports.wbg.__wbg___wbindgen_is_null_dfda7d66506c95b5 = function(arg0) {
        const ret = getObject(arg0) === null;
        return ret;
    };
    imports.wbg.__wbg___wbindgen_is_object_ce774f3490692386 = function(arg0) {
        const val = getObject(arg0);
        const ret = typeof(val) === 'object' && val !== null;
        return ret;
    };
    imports.wbg.__wbg___wbindgen_is_string_704ef9c8fc131030 = function(arg0) {
        const ret = typeof(getObject(arg0)) === 'string';
        return ret;
    };
    imports.wbg.__wbg___wbindgen_is_undefined_f6b95eab589e0269 = function(arg0) {
        const ret = getObject(arg0) === undefined;
        return ret;
    };
    imports.wbg.__wbg___wbindgen_number_get_9619185a74197f95 = function(arg0, arg1) {
        const obj = getObject(arg1);
        const ret = typeof(obj) === 'number' ? obj : undefined;
        getDataViewMemory0().setFloat64(arg0 + 8 * 1, isLikeNone(ret) ? 0 : ret, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, !isLikeNone(ret), true);
    };
    imports.wbg.__wbg___wbindgen_rethrow_78714972834ecdf1 = function(arg0) {
        throw takeObject(arg0);
    };
    imports.wbg.__wbg___wbindgen_string_get_a2a31e16edf96e42 = function(arg0, arg1) {
        const obj = getObject(arg1);
        const ret = typeof(obj) === 'string' ? obj : undefined;
        var ptr1 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        var len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    };
    imports.wbg.__wbg___wbindgen_throw_dd24417ed36fc46e = function(arg0, arg1) {
        var v0 = getCachedStringFromWasm0(arg0, arg1);
        throw new Error(v0);
    };
    imports.wbg.__wbg__wbg_cb_unref_87dfb5aaa0cbcea7 = function(arg0) {
        getObject(arg0)._wbg_cb_unref();
    };
    imports.wbg.__wbg_addEventListener_6a82629b3d430a48 = function() { return handleError(function (arg0, arg1, arg2, arg3) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).addEventListener(v0, getObject(arg3));
    }, arguments) };
    imports.wbg.__wbg_addEventListener_82cddc614107eb45 = function() { return handleError(function (arg0, arg1, arg2, arg3, arg4) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).addEventListener(v0, getObject(arg3), getObject(arg4));
    }, arguments) };
    imports.wbg.__wbg_addIceCandidate_a2f1c667d2083f21 = function(arg0, arg1) {
        const ret = getObject(arg0).addIceCandidate(getObject(arg1));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_add_a928536d6ee293f3 = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).add(v0);
    }, arguments) };
    imports.wbg.__wbg_appendChild_7465eba84213c75f = function() { return handleError(function (arg0, arg1) {
        const ret = getObject(arg0).appendChild(getObject(arg1));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_attributeName_0145ba2757e06e60 = function(arg0, arg1) {
        const ret = getObject(arg1).attributeName;
        var ptr1 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        var len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    };
    imports.wbg.__wbg_body_544738f8b03aef13 = function(arg0) {
        const ret = getObject(arg0).body;
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_bottom_7d2cabadf8c452a8 = function(arg0) {
        const ret = getObject(arg0).bottom;
        return ret;
    };
    imports.wbg.__wbg_bufferedAmount_865f54a4c041c8cb = function(arg0) {
        const ret = getObject(arg0).bufferedAmount;
        return ret;
    };
    imports.wbg.__wbg_bufferedAmount_a7b776e2edc9677b = function(arg0) {
        const ret = getObject(arg0).bufferedAmount;
        return ret;
    };
    imports.wbg.__wbg_call_3020136f7a2d6e44 = function() { return handleError(function (arg0, arg1, arg2) {
        const ret = getObject(arg0).call(getObject(arg1), getObject(arg2));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_call_abb4ff46ce38be40 = function() { return handleError(function (arg0, arg1) {
        const ret = getObject(arg0).call(getObject(arg1));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_call_c8baa5c5e72d274e = function() { return handleError(function (arg0, arg1, arg2, arg3) {
        const ret = getObject(arg0).call(getObject(arg1), getObject(arg2), getObject(arg3));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_candidate_4e3d572df5288462 = function(arg0, arg1) {
        const ret = getObject(arg1).candidate;
        const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    };
    imports.wbg.__wbg_candidate_d3b7043104cac524 = function(arg0) {
        const ret = getObject(arg0).candidate;
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_channel_798e703f3dd88dc4 = function(arg0) {
        const ret = getObject(arg0).channel;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_classList_d75bc19322d1b8f4 = function(arg0) {
        const ret = getObject(arg0).classList;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_clearInterval_04be3b7c0d091a56 = function(arg0, arg1) {
        getObject(arg0).clearInterval(arg1);
    };
    imports.wbg.__wbg_clearInterval_45ac4607741420fd = function(arg0, arg1) {
        getObject(arg0).clearInterval(arg1);
    };
    imports.wbg.__wbg_clearTimeout_5a54f8841c30079a = function(arg0) {
        const ret = clearTimeout(takeObject(arg0));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_clearTimeout_96804de0ab838f26 = function(arg0) {
        const ret = clearTimeout(takeObject(arg0));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_clientHeight_2554d64d9b2dfc4f = function(arg0) {
        const ret = getObject(arg0).clientHeight;
        return ret;
    };
    imports.wbg.__wbg_clientX_c17906c33ea43025 = function(arg0) {
        const ret = getObject(arg0).clientX;
        return ret;
    };
    imports.wbg.__wbg_clientY_70eb66d231a332a3 = function(arg0) {
        const ret = getObject(arg0).clientY;
        return ret;
    };
    imports.wbg.__wbg_clipboard_c210ce30f20907dd = function(arg0) {
        const ret = getObject(arg0).clipboard;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_close_6403f219587f55fb = function(arg0) {
        getObject(arg0).close();
    };
    imports.wbg.__wbg_close_9cdab4afe1eeaf53 = function(arg0) {
        getObject(arg0).close();
    };
    imports.wbg.__wbg_close_ff2e6995683b2ad9 = function() { return handleError(function (arg0, arg1, arg2, arg3) {
        var v0 = getCachedStringFromWasm0(arg2, arg3);
        getObject(arg0).close(arg1, v0);
    }, arguments) };
    imports.wbg.__wbg_connect_f28a2db518e02462 = function() { return handleError(function (arg0, arg1) {
        const ret = getObject(arg0).connect(getObject(arg1));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_connectionState_6cfc28903ca479ea = function(arg0) {
        const ret = getObject(arg0).connectionState;
        return (__wbindgen_enum_RtcPeerConnectionState.indexOf(ret) + 1 || 7) - 1;
    };
    imports.wbg.__wbg_createAnswer_57fa5e0880a7b92a = function(arg0) {
        const ret = getObject(arg0).createAnswer();
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_createComment_89db599aa930ef8a = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).createComment(v0);
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_createDataChannel_524f06cbf5ce9db6 = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).createDataChannel(v0);
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_createDataChannel_a68737dabcdb016a = function(arg0, arg1, arg2, arg3) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).createDataChannel(v0, getObject(arg3));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_createElementNS_e7c12bbd579529e2 = function() { return handleError(function (arg0, arg1, arg2, arg3, arg4) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        var v1 = getCachedStringFromWasm0(arg3, arg4);
        const ret = getObject(arg0).createElementNS(v0, v1);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_createElement_da4ed2b219560fc6 = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).createElement(v0);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_createMediaStreamSource_bad9dbe1d85c3cb7 = function() { return handleError(function (arg0, arg1) {
        const ret = getObject(arg0).createMediaStreamSource(getObject(arg1));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_createObjectStore_dba64acfe84d4191 = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).createObjectStore(v0);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_createOffer_bb9103bcea24bcec = function(arg0) {
        const ret = getObject(arg0).createOffer();
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_createScriptProcessor_e531a9ed96fc97bb = function() { return handleError(function (arg0, arg1, arg2, arg3) {
        const ret = getObject(arg0).createScriptProcessor(arg1 >>> 0, arg2 >>> 0, arg3 >>> 0);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_createTextNode_0cf8168f7646a5d2 = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).createTextNode(v0);
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_crypto_574e78ad8b13b65f = function(arg0) {
        const ret = getObject(arg0).crypto;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_dataTransfer_3653d4b679b026b2 = function(arg0) {
        const ret = getObject(arg0).dataTransfer;
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_data_0886c3d89129fac9 = function() { return handleError(function (arg0, arg1) {
        const ret = getObject(arg1).data;
        const ptr1 = passArray8ToWasm0(ret, wasm.__wbindgen_export);
        const len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    }, arguments) };
    imports.wbg.__wbg_data_8bf4ae669a78a688 = function(arg0) {
        const ret = getObject(arg0).data;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_destination_1dd37feba0c0cab6 = function(arg0) {
        const ret = getObject(arg0).destination;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_detectPitch_341b53591d3a662b = function(arg0, arg1) {
        const ret = detectPitch(getArrayF32FromWasm0(arg0, arg1));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_disconnect_73648182b9afde22 = function() { return handleError(function (arg0) {
        getObject(arg0).disconnect();
    }, arguments) };
    imports.wbg.__wbg_document_5b745e82ba551ca5 = function(arg0) {
        const ret = getObject(arg0).document;
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_done_62ea16af4ce34b24 = function(arg0) {
        const ret = getObject(arg0).done;
        return ret;
    };
    imports.wbg.__wbg_error_7534b8e9a36f1ab4 = function(arg0, arg1) {
        var v0 = getCachedStringFromWasm0(arg0, arg1);
        if (arg0 !== 0) { wasm.__wbindgen_export4(arg0, arg1, 1); }
        console.error(v0);
    };
    imports.wbg.__wbg_error_7bc7d576a6aaf855 = function(arg0) {
        console.error(getObject(arg0));
    };
    imports.wbg.__wbg_from_29a8414a7a7cd19d = function(arg0) {
        const ret = Array.from(getObject(arg0));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_generateCertificate_61f8d8f69210aa07 = function() { return handleError(function (arg0) {
        const ret = RTCPeerConnection.generateCertificate(getObject(arg0));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_getAttribute_80900eec94cb3636 = function(arg0, arg1, arg2, arg3) {
        var v0 = getCachedStringFromWasm0(arg2, arg3);
        const ret = getObject(arg1).getAttribute(v0);
        var ptr2 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        var len2 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len2, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr2, true);
    };
    imports.wbg.__wbg_getBoundingClientRect_25e44a78507968b0 = function(arg0) {
        const ret = getObject(arg0).getBoundingClientRect();
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_getChannelData_bae3cd34129a8fef = function() { return handleError(function (arg0, arg1, arg2) {
        const ret = getObject(arg1).getChannelData(arg2 >>> 0);
        const ptr1 = passArrayF32ToWasm0(ret, wasm.__wbindgen_export);
        const len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    }, arguments) };
    imports.wbg.__wbg_getData_0c18014d58e42433 = function() { return handleError(function (arg0, arg1, arg2, arg3) {
        var v0 = getCachedStringFromWasm0(arg2, arg3);
        const ret = getObject(arg1).getData(v0);
        const ptr2 = passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len2 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len2, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr2, true);
    }, arguments) };
    imports.wbg.__wbg_getElementById_e05488d2143c2b21 = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).getElementById(v0);
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_getRandomValues_1c61fac11405ffdc = function() { return handleError(function (arg0, arg1) {
        globalThis.crypto.getRandomValues(getArrayU8FromWasm0(arg0, arg1));
    }, arguments) };
    imports.wbg.__wbg_getRandomValues_b8f5dbd5f3995a9e = function() { return handleError(function (arg0, arg1) {
        getObject(arg0).getRandomValues(getObject(arg1));
    }, arguments) };
    imports.wbg.__wbg_getTracks_75ecd4c89e587a44 = function(arg0) {
        const ret = getObject(arg0).getTracks();
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_getUserMedia_6c26923b30317a2e = function() { return handleError(function (arg0, arg1) {
        const ret = getObject(arg0).getUserMedia(getObject(arg1));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_get_6b7bd52aca3f9671 = function(arg0, arg1) {
        const ret = getObject(arg0)[arg1 >>> 0];
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_get_7d8b665fa88606d5 = function() { return handleError(function (arg0, arg1) {
        const ret = getObject(arg0).get(getObject(arg1));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_get_af9dab7e9603ea93 = function() { return handleError(function (arg0, arg1) {
        const ret = Reflect.get(getObject(arg0), getObject(arg1));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_get_c53d381635aa3929 = function(arg0, arg1) {
        const ret = getObject(arg0)[arg1 >>> 0];
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_hasAttribute_af746bd1e7f1b334 = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).hasAttribute(v0);
        return ret;
    };
    imports.wbg.__wbg_hash_979a7861415bf1f8 = function() { return handleError(function (arg0, arg1) {
        const ret = getObject(arg1).hash;
        const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    }, arguments) };
    imports.wbg.__wbg_iceConnectionState_aa66153d58e4b3b5 = function(arg0) {
        const ret = getObject(arg0).iceConnectionState;
        return (__wbindgen_enum_RtcIceConnectionState.indexOf(ret) + 1 || 8) - 1;
    };
    imports.wbg.__wbg_iceGatheringState_6b243c9b32142b25 = function(arg0) {
        const ret = getObject(arg0).iceGatheringState;
        return (__wbindgen_enum_RtcIceGatheringState.indexOf(ret) + 1 || 4) - 1;
    };
    imports.wbg.__wbg_indexedDB_23c232e00a1e28ad = function() { return handleError(function (arg0) {
        const ret = getObject(arg0).indexedDB;
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_initSwiftF0_486d637b254f7aef = function() { return handleError(function () {
        const ret = initSwiftF0();
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_inputBuffer_12e7fea36ca7ffe4 = function() { return handleError(function (arg0) {
        const ret = getObject(arg0).inputBuffer;
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_inputs_f70b0d8f2b041cfe = function(arg0) {
        const ret = getObject(arg0).inputs;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_insertBefore_93e77c32aeae9657 = function() { return handleError(function (arg0, arg1, arg2) {
        const ret = getObject(arg0).insertBefore(getObject(arg1), getObject(arg2));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_instanceof_Error_3443650560328fa9 = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof Error;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_instanceof_HtmlElement_20a3acb594113d73 = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof HTMLElement;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_instanceof_HtmlInputElement_46b31917ce88698f = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof HTMLInputElement;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_instanceof_HtmlSelectElement_7b43062ce94166ee = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof HTMLSelectElement;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_instanceof_MediaStreamTrack_8511b4a35e6bfbf1 = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof MediaStreamTrack;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_instanceof_MediaStream_d683650ea5ecc979 = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof MediaStream;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_instanceof_MidiAccess_6d74331716f7c644 = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof MIDIAccess;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_instanceof_MidiInput_6943736151f9b596 = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof MIDIInput;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_instanceof_MidiOutput_7b0a891bc9f7c86b = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof MIDIOutput;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_instanceof_MutationRecord_9701e6e17b6e3596 = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof MutationRecord;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_instanceof_RtcPeerConnection_44a166a58e64d042 = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof RTCPeerConnection;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_instanceof_Window_b5cf7783caa68180 = function(arg0) {
        let result;
        try {
            result = getObject(arg0) instanceof Window;
        } catch (_) {
            result = false;
        }
        const ret = result;
        return ret;
    };
    imports.wbg.__wbg_isArray_51fd9e6422c0a395 = function(arg0) {
        const ret = Array.isArray(getObject(arg0));
        return ret;
    };
    imports.wbg.__wbg_item_dee347a1a2592eee = function(arg0, arg1) {
        const ret = getObject(arg0).item(arg1 >>> 0);
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_iterator_27b7c8b35ab3e86b = function() {
        const ret = Symbol.iterator;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_key_505d33c50799526a = function(arg0, arg1) {
        const ret = getObject(arg1).key;
        const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    };
    imports.wbg.__wbg_left_d52bfa3824286825 = function(arg0) {
        const ret = getObject(arg0).left;
        return ret;
    };
    imports.wbg.__wbg_length_22ac23eaec9d8053 = function(arg0) {
        const ret = getObject(arg0).length;
        return ret;
    };
    imports.wbg.__wbg_length_b2fe2857226439bf = function(arg0) {
        const ret = getObject(arg0).length;
        return ret;
    };
    imports.wbg.__wbg_length_d45040a40c570362 = function(arg0) {
        const ret = getObject(arg0).length;
        return ret;
    };
    imports.wbg.__wbg_location_962e75c1c1b3ebed = function(arg0) {
        const ret = getObject(arg0).location;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_log_0cc1b7768397bcfe = function(arg0, arg1, arg2, arg3, arg4, arg5, arg6, arg7) {
        var v0 = getCachedStringFromWasm0(arg0, arg1);
        if (arg0 !== 0) { wasm.__wbindgen_export4(arg0, arg1, 1); }
        var v1 = getCachedStringFromWasm0(arg2, arg3);
        var v2 = getCachedStringFromWasm0(arg4, arg5);
        var v3 = getCachedStringFromWasm0(arg6, arg7);
        console.log(v0, v1, v2, v3);
    };
    imports.wbg.__wbg_log_1d990106d99dacb7 = function(arg0) {
        console.log(getObject(arg0));
    };
    imports.wbg.__wbg_log_cb9e190acc5753fb = function(arg0, arg1) {
        var v0 = getCachedStringFromWasm0(arg0, arg1);
        if (arg0 !== 0) { wasm.__wbindgen_export4(arg0, arg1, 1); }
        console.log(v0);
    };
    imports.wbg.__wbg_mark_7438147ce31e9d4b = function(arg0, arg1) {
        var v0 = getCachedStringFromWasm0(arg0, arg1);
        performance.mark(v0);
    };
    imports.wbg.__wbg_measure_fb7825c11612c823 = function() { return handleError(function (arg0, arg1, arg2, arg3) {
        var v0 = getCachedStringFromWasm0(arg0, arg1);
        if (arg0 !== 0) { wasm.__wbindgen_export4(arg0, arg1, 1); }
        var v1 = getCachedStringFromWasm0(arg2, arg3);
        if (arg2 !== 0) { wasm.__wbindgen_export4(arg2, arg3, 1); }
        performance.measure(v0, v1);
    }, arguments) };
    imports.wbg.__wbg_mediaDevices_9cbe26ce22d56511 = function() { return handleError(function (arg0) {
        const ret = getObject(arg0).mediaDevices;
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_msCrypto_a61aeb35a24c1329 = function(arg0) {
        const ret = getObject(arg0).msCrypto;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_name_710097b250a33411 = function(arg0, arg1) {
        const ret = getObject(arg1).name;
        var ptr1 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        var len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    };
    imports.wbg.__wbg_navigator_b49edef831236138 = function(arg0) {
        const ret = getObject(arg0).navigator;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_new_1ba21ce319a06297 = function() {
        const ret = new Object();
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_new_25f239778d6112b9 = function() {
        const ret = new Array();
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_new_51fd2455559dc156 = function() { return handleError(function (arg0) {
        const ret = new MutationObserver(getObject(arg0));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_new_5e542c992f14cb6f = function() { return handleError(function () {
        const ret = new lAudioContext();
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_new_6421f6084cc5bc5a = function(arg0) {
        const ret = new Uint8Array(getObject(arg0));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_new_7c30d1f874652e62 = function() { return handleError(function (arg0, arg1) {
        var v0 = getCachedStringFromWasm0(arg0, arg1);
        const ret = new WebSocket(v0);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_new_8a6f238a6ece86ea = function() {
        const ret = new Error();
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_new_ff12d2b041fb48f1 = function(arg0, arg1) {
        try {
            var state0 = {a: arg0, b: arg1};
            var cb0 = (arg0, arg1) => {
                const a = state0.a;
                state0.a = 0;
                try {
                    return __wasm_bindgen_func_elem_13511(a, state0.b, arg0, arg1);
                } finally {
                    state0.a = a;
                }
            };
            const ret = new Promise(cb0);
            return addHeapObject(ret);
        } finally {
            state0.a = state0.b = 0;
        }
    };
    imports.wbg.__wbg_new_from_slice_f9c22b9153b26992 = function(arg0, arg1) {
        const ret = new Uint8Array(getArrayU8FromWasm0(arg0, arg1));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_new_no_args_cb138f77cf6151ee = function(arg0, arg1) {
        var v0 = getCachedStringFromWasm0(arg0, arg1);
        const ret = new Function(v0);
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_new_with_configuration_d8c11e79765332b1 = function() { return handleError(function (arg0) {
        const ret = new RTCPeerConnection(getObject(arg0));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_new_with_length_aa5eaf41d35235e5 = function(arg0) {
        const ret = new Uint8Array(arg0 >>> 0);
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_next_138a17bbf04e926c = function(arg0) {
        const ret = getObject(arg0).next;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_next_3cfe5c0fe2a4cc53 = function() { return handleError(function (arg0) {
        const ret = getObject(arg0).next();
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_node_905d3e251edff8a2 = function(arg0) {
        const ret = getObject(arg0).node;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_now_2c95c9de01293173 = function(arg0) {
        const ret = getObject(arg0).now();
        return ret;
    };
    imports.wbg.__wbg_now_69d776cd24f5215b = function() {
        const ret = Date.now();
        return ret;
    };
    imports.wbg.__wbg_objectStore_da9a077b8849dbe9 = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).objectStore(v0);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_observe_ecd069ee6d103261 = function() { return handleError(function (arg0, arg1, arg2) {
        getObject(arg0).observe(getObject(arg1), getObject(arg2));
    }, arguments) };
    imports.wbg.__wbg_of_6505a0eb509da02e = function(arg0) {
        const ret = Array.of(getObject(arg0));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_open_0d7b85f4c0a38ffe = function() { return handleError(function (arg0, arg1, arg2, arg3) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).open(v0, arg3 >>> 0);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_outputs_6b55d06db39aaa88 = function(arg0) {
        const ret = getObject(arg0).outputs;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_performance_7a3ffd0b17f663ad = function(arg0) {
        const ret = getObject(arg0).performance;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_pointerId_bf4326e151df1474 = function(arg0) {
        const ret = getObject(arg0).pointerId;
        return ret;
    };
    imports.wbg.__wbg_preventDefault_e97663aeeb9709d3 = function(arg0) {
        getObject(arg0).preventDefault();
    };
    imports.wbg.__wbg_process_dc0fbacc7c1c06f7 = function(arg0) {
        const ret = getObject(arg0).process;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_prototypesetcall_dfe9b766cdc1f1fd = function(arg0, arg1, arg2) {
        Uint8Array.prototype.set.call(getArrayU8FromWasm0(arg0, arg1), getObject(arg2));
    };
    imports.wbg.__wbg_push_7d9be8f38fc13975 = function(arg0, arg1) {
        const ret = getObject(arg0).push(getObject(arg1));
        return ret;
    };
    imports.wbg.__wbg_put_d40a68e5a8902a46 = function() { return handleError(function (arg0, arg1, arg2) {
        const ret = getObject(arg0).put(getObject(arg1), getObject(arg2));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_querySelectorAll_0f71ea37892a269b = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).querySelectorAll(v0);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_querySelectorAll_aa1048eae18f6f1a = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).querySelectorAll(v0);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_querySelector_15a92ce6bed6157d = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).querySelector(v0);
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_querySelector_83602705c2df3db0 = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).querySelector(v0);
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_queueMicrotask_9b549dfce8865860 = function(arg0) {
        const ret = getObject(arg0).queueMicrotask;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_queueMicrotask_fca69f5bfad613a5 = function(arg0) {
        queueMicrotask(getObject(arg0));
    };
    imports.wbg.__wbg_randomFillSync_ac0988aba3254290 = function() { return handleError(function (arg0, arg1) {
        getObject(arg0).randomFillSync(takeObject(arg1));
    }, arguments) };
    imports.wbg.__wbg_readyState_4a98e3f4691d8e5e = function(arg0) {
        const ret = getObject(arg0).readyState;
        return (__wbindgen_enum_RtcDataChannelState.indexOf(ret) + 1 || 5) - 1;
    };
    imports.wbg.__wbg_readyState_9d0976dcad561aa9 = function(arg0) {
        const ret = getObject(arg0).readyState;
        return ret;
    };
    imports.wbg.__wbg_releasePointerCapture_83b7fe5b3ce3eebb = function() { return handleError(function (arg0, arg1) {
        getObject(arg0).releasePointerCapture(arg1);
    }, arguments) };
    imports.wbg.__wbg_removeAttribute_96e791ceeb22d591 = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).removeAttribute(v0);
    }, arguments) };
    imports.wbg.__wbg_removeChild_e269b93f63c5ba71 = function() { return handleError(function (arg0, arg1) {
        const ret = getObject(arg0).removeChild(getObject(arg1));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_removeEventListener_3ff68cd2edbc58d4 = function() { return handleError(function (arg0, arg1, arg2, arg3, arg4) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).removeEventListener(v0, getObject(arg3), arg4 !== 0);
    }, arguments) };
    imports.wbg.__wbg_removeProperty_c2e16faee2834bef = function() { return handleError(function (arg0, arg1, arg2, arg3) {
        var v0 = getCachedStringFromWasm0(arg2, arg3);
        const ret = getObject(arg1).removeProperty(v0);
        const ptr2 = passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len2 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len2, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr2, true);
    }, arguments) };
    imports.wbg.__wbg_remove_32f69ffabcbc4072 = function(arg0) {
        getObject(arg0).remove();
    };
    imports.wbg.__wbg_remove_c705a65e04542a70 = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).remove(v0);
    }, arguments) };
    imports.wbg.__wbg_replaceChild_d6f78ac48f46aedf = function() { return handleError(function (arg0, arg1, arg2) {
        const ret = getObject(arg0).replaceChild(getObject(arg1), getObject(arg2));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_requestMIDIAccess_3362d3a3e0c5cc1a = function() { return handleError(function (arg0, arg1) {
        const ret = getObject(arg0).requestMIDIAccess(getObject(arg1));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_require_60cc747a6bc5215a = function() { return handleError(function () {
        const ret = module.require;
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_resolve_fd5bfbaa4ce36e1e = function(arg0) {
        const ret = Promise.resolve(getObject(arg0));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_result_084f962aedb54250 = function() { return handleError(function (arg0) {
        const ret = getObject(arg0).result;
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_resume_7d320c513be492d2 = function() { return handleError(function (arg0) {
        const ret = getObject(arg0).resume();
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_right_b7a0dcdcec0301e1 = function(arg0) {
        const ret = getObject(arg0).right;
        return ret;
    };
    imports.wbg.__wbg_scrollTop_76691208c28906d5 = function(arg0) {
        const ret = getObject(arg0).scrollTop;
        return ret;
    };
    imports.wbg.__wbg_search_856af82f9dccb2ef = function() { return handleError(function (arg0, arg1) {
        const ret = getObject(arg1).search;
        const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    }, arguments) };
    imports.wbg.__wbg_send_6f4153e7a2887f5f = function() { return handleError(function (arg0, arg1, arg2) {
        getObject(arg0).send(getArrayU8FromWasm0(arg1, arg2));
    }, arguments) };
    imports.wbg.__wbg_send_bfc4e55f80bcd338 = function() { return handleError(function (arg0, arg1) {
        getObject(arg0).send(getObject(arg1));
    }, arguments) };
    imports.wbg.__wbg_send_ea59e150ab5ebe08 = function() { return handleError(function (arg0, arg1, arg2) {
        getObject(arg0).send(getArrayU8FromWasm0(arg1, arg2));
    }, arguments) };
    imports.wbg.__wbg_setAttribute_34747dd193f45828 = function() { return handleError(function (arg0, arg1, arg2, arg3, arg4) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        var v1 = getCachedStringFromWasm0(arg3, arg4);
        getObject(arg0).setAttribute(v0, v1);
    }, arguments) };
    imports.wbg.__wbg_setData_b56422b286b18ff6 = function() { return handleError(function (arg0, arg1, arg2, arg3, arg4) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        var v1 = getCachedStringFromWasm0(arg3, arg4);
        getObject(arg0).setData(v0, v1);
    }, arguments) };
    imports.wbg.__wbg_setInterval_50126577e843eccd = function() { return handleError(function (arg0, arg1, arg2, arg3) {
        const ret = getObject(arg0).setInterval(getObject(arg1), arg2, ...getObject(arg3));
        return ret;
    }, arguments) };
    imports.wbg.__wbg_setInterval_a344b5bccdd6075a = function() { return handleError(function (arg0, arg1, arg2, arg3) {
        const ret = getObject(arg0).setInterval(getObject(arg1), arg2, ...getObject(arg3));
        return ret;
    }, arguments) };
    imports.wbg.__wbg_setLocalDescription_b2b733aef9d90b85 = function(arg0, arg1) {
        const ret = getObject(arg0).setLocalDescription(getObject(arg1));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_setPointerCapture_c611f4bcb7e9081e = function() { return handleError(function (arg0, arg1) {
        getObject(arg0).setPointerCapture(arg1);
    }, arguments) };
    imports.wbg.__wbg_setProperty_f27b2c05323daf8a = function() { return handleError(function (arg0, arg1, arg2, arg3, arg4) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        var v1 = getCachedStringFromWasm0(arg3, arg4);
        getObject(arg0).setProperty(v0, v1);
    }, arguments) };
    imports.wbg.__wbg_setRemoteDescription_2678c3c1d5e054e5 = function(arg0, arg1) {
        const ret = getObject(arg0).setRemoteDescription(getObject(arg1));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_setTimeout_06477c23d31efef1 = function() { return handleError(function (arg0, arg1, arg2) {
        const ret = getObject(arg0).setTimeout(getObject(arg1), arg2);
        return ret;
    }, arguments) };
    imports.wbg.__wbg_setTimeout_db2dbaeefb6f39c7 = function() { return handleError(function (arg0, arg1) {
        const ret = setTimeout(getObject(arg0), arg1);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_setTimeout_eefe7f4c234b0c6b = function() { return handleError(function (arg0, arg1) {
        const ret = setTimeout(getObject(arg0), arg1);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_set_781438a03c0c3c81 = function() { return handleError(function (arg0, arg1, arg2) {
        const ret = Reflect.set(getObject(arg0), getObject(arg1), getObject(arg2));
        return ret;
    }, arguments) };
    imports.wbg.__wbg_set_attribute_filter_071f8435d4957423 = function(arg0, arg1) {
        getObject(arg0).attributeFilter = getObject(arg1);
    };
    imports.wbg.__wbg_set_attributes_cb8a8ae04435f622 = function(arg0, arg1) {
        getObject(arg0).attributes = arg1 !== 0;
    };
    imports.wbg.__wbg_set_audio_05d7278e0f8f36e1 = function(arg0, arg1) {
        getObject(arg0).audio = getObject(arg1);
    };
    imports.wbg.__wbg_set_binaryType_107d1c9c7003907b = function(arg0, arg1) {
        getObject(arg0).binaryType = __wbindgen_enum_RtcDataChannelType[arg1];
    };
    imports.wbg.__wbg_set_binaryType_73e8c75df97825f8 = function(arg0, arg1) {
        getObject(arg0).binaryType = __wbindgen_enum_BinaryType[arg1];
    };
    imports.wbg.__wbg_set_bufferedAmountLowThreshold_00d87dd7474e86f2 = function(arg0, arg1) {
        getObject(arg0).bufferedAmountLowThreshold = arg1 >>> 0;
    };
    imports.wbg.__wbg_set_candidate_a300596d8311ab98 = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).candidate = v0;
    };
    imports.wbg.__wbg_set_capture_0bafa9ad80668352 = function(arg0, arg1) {
        getObject(arg0).capture = arg1 !== 0;
    };
    imports.wbg.__wbg_set_certificates_0e2919861873c5e7 = function(arg0, arg1) {
        getObject(arg0).certificates = getObject(arg1);
    };
    imports.wbg.__wbg_set_className_52411a7e4782668d = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).className = v0;
    };
    imports.wbg.__wbg_set_data_c2a06f99ad40fc56 = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).data = v0;
    };
    imports.wbg.__wbg_set_effectAllowed_f9573b9a06724490 = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).effectAllowed = v0;
    };
    imports.wbg.__wbg_set_hash_28b98ad4bd1349f4 = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).hash = v0;
    }, arguments) };
    imports.wbg.__wbg_set_ice_servers_7aa5a25622397c52 = function(arg0, arg1) {
        getObject(arg0).iceServers = getObject(arg1);
    };
    imports.wbg.__wbg_set_id_702da6e1bcec3b45 = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).id = v0;
    };
    imports.wbg.__wbg_set_id_ecddaebcf0b4b860 = function(arg0, arg1) {
        getObject(arg0).id = arg1;
    };
    imports.wbg.__wbg_set_negotiated_689501f71df13f78 = function(arg0, arg1) {
        getObject(arg0).negotiated = arg1 !== 0;
    };
    imports.wbg.__wbg_set_onaudioprocess_e714a59e2c2376cb = function(arg0, arg1) {
        getObject(arg0).onaudioprocess = getObject(arg1);
    };
    imports.wbg.__wbg_set_onbufferedamountlow_62eb7887b1e6794c = function(arg0, arg1) {
        getObject(arg0).onbufferedamountlow = getObject(arg1);
    };
    imports.wbg.__wbg_set_once_cb88c6a887803dfa = function(arg0, arg1) {
        getObject(arg0).once = arg1 !== 0;
    };
    imports.wbg.__wbg_set_onclose_032729b3d7ed7a9e = function(arg0, arg1) {
        getObject(arg0).onclose = getObject(arg1);
    };
    imports.wbg.__wbg_set_onclose_09e9cb7e437bcaae = function(arg0, arg1) {
        getObject(arg0).onclose = getObject(arg1);
    };
    imports.wbg.__wbg_set_onconnectionstatechange_e1dbddc82b89847b = function(arg0, arg1) {
        getObject(arg0).onconnectionstatechange = getObject(arg1);
    };
    imports.wbg.__wbg_set_ondatachannel_1618a8c6c27b0e13 = function(arg0, arg1) {
        getObject(arg0).ondatachannel = getObject(arg1);
    };
    imports.wbg.__wbg_set_onerror_08fecec3bdc9d24d = function(arg0, arg1) {
        getObject(arg0).onerror = getObject(arg1);
    };
    imports.wbg.__wbg_set_onerror_7819daa6af176ddb = function(arg0, arg1) {
        getObject(arg0).onerror = getObject(arg1);
    };
    imports.wbg.__wbg_set_onicecandidate_4a07fd3142f54596 = function(arg0, arg1) {
        getObject(arg0).onicecandidate = getObject(arg1);
    };
    imports.wbg.__wbg_set_oniceconnectionstatechange_a18f4db27217e889 = function(arg0, arg1) {
        getObject(arg0).oniceconnectionstatechange = getObject(arg1);
    };
    imports.wbg.__wbg_set_onicegatheringstatechange_1faf8b3269759de9 = function(arg0, arg1) {
        getObject(arg0).onicegatheringstatechange = getObject(arg1);
    };
    imports.wbg.__wbg_set_onmessage_103954d025ec39fd = function(arg0, arg1) {
        getObject(arg0).onmessage = getObject(arg1);
    };
    imports.wbg.__wbg_set_onmessage_71321d0bed69856c = function(arg0, arg1) {
        getObject(arg0).onmessage = getObject(arg1);
    };
    imports.wbg.__wbg_set_onmidimessage_6e5a046ff7fb5068 = function(arg0, arg1) {
        getObject(arg0).onmidimessage = getObject(arg1);
    };
    imports.wbg.__wbg_set_onopen_67a895fd2807a682 = function(arg0, arg1) {
        getObject(arg0).onopen = getObject(arg1);
    };
    imports.wbg.__wbg_set_onopen_6d4abedb27ba5656 = function(arg0, arg1) {
        getObject(arg0).onopen = getObject(arg1);
    };
    imports.wbg.__wbg_set_onsignalingstatechange_d3b3b70de596a1da = function(arg0, arg1) {
        getObject(arg0).onsignalingstatechange = getObject(arg1);
    };
    imports.wbg.__wbg_set_onstatechange_4aad3045e212e3e7 = function(arg0, arg1) {
        getObject(arg0).onstatechange = getObject(arg1);
    };
    imports.wbg.__wbg_set_onsuccess_94332a00452de699 = function(arg0, arg1) {
        getObject(arg0).onsuccess = getObject(arg1);
    };
    imports.wbg.__wbg_set_onupgradeneeded_3dc6e233a6d13fe2 = function(arg0, arg1) {
        getObject(arg0).onupgradeneeded = getObject(arg1);
    };
    imports.wbg.__wbg_set_passive_a3aa35eb7292414e = function(arg0, arg1) {
        getObject(arg0).passive = arg1 !== 0;
    };
    imports.wbg.__wbg_set_sdp_8a58fb4588ae8dfe = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).sdp = v0;
    };
    imports.wbg.__wbg_set_sdp_m_line_index_bdca6129097ebe22 = function(arg0, arg1) {
        getObject(arg0).sdpMLineIndex = arg1 === 0xFFFFFF ? undefined : arg1;
    };
    imports.wbg.__wbg_set_sdp_mid_0e314047c91cf316 = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).sdpMid = v0;
    };
    imports.wbg.__wbg_set_textContent_e461237efe237e01 = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).textContent = v0;
    };
    imports.wbg.__wbg_set_type_966bfe79c94c1a20 = function(arg0, arg1) {
        getObject(arg0).type = __wbindgen_enum_RtcSdpType[arg1];
    };
    imports.wbg.__wbg_set_url_92c09098fdc60d05 = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        getObject(arg0).url = v0;
    };
    imports.wbg.__wbg_set_video_26d356cfa8f70f76 = function(arg0, arg1) {
        getObject(arg0).video = getObject(arg1);
    };
    imports.wbg.__wbg_signalingState_348ee2e74064e6a4 = function(arg0) {
        const ret = getObject(arg0).signalingState;
        return (__wbindgen_enum_RtcSignalingState.indexOf(ret) + 1 || 7) - 1;
    };
    imports.wbg.__wbg_stack_0ed75d68575b0f3c = function(arg0, arg1) {
        const ret = getObject(arg1).stack;
        const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    };
    imports.wbg.__wbg_state_c28a610878aaf489 = function(arg0) {
        const ret = getObject(arg0).state;
        return (__wbindgen_enum_AudioContextState.indexOf(ret) + 1 || 4) - 1;
    };
    imports.wbg.__wbg_static_accessor_GLOBAL_769e6b65d6557335 = function() {
        const ret = typeof global === 'undefined' ? null : global;
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_static_accessor_GLOBAL_THIS_60cf02db4de8e1c1 = function() {
        const ret = typeof globalThis === 'undefined' ? null : globalThis;
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_static_accessor_SELF_08f5a74c69739274 = function() {
        const ret = typeof self === 'undefined' ? null : self;
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_static_accessor_WINDOW_a8924b26aa92d024 = function() {
        const ret = typeof window === 'undefined' ? null : window;
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_stopPropagation_611935c25ee35a3c = function(arg0) {
        getObject(arg0).stopPropagation();
    };
    imports.wbg.__wbg_stop_8a61abecefef6af3 = function(arg0) {
        getObject(arg0).stop();
    };
    imports.wbg.__wbg_stringify_655a6390e1f5eb6b = function() { return handleError(function (arg0) {
        const ret = JSON.stringify(getObject(arg0));
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_style_521a717da50e53c6 = function(arg0) {
        const ret = getObject(arg0).style;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_subarray_845f2f5bce7d061a = function(arg0, arg1, arg2) {
        const ret = getObject(arg0).subarray(arg1 >>> 0, arg2 >>> 0);
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_target_0e3e05a6263c37a0 = function(arg0) {
        const ret = getObject(arg0).target;
        return isLikeNone(ret) ? 0 : addHeapObject(ret);
    };
    imports.wbg.__wbg_then_429f7caf1026411d = function(arg0, arg1, arg2) {
        const ret = getObject(arg0).then(getObject(arg1), getObject(arg2));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_then_4f95312d68691235 = function(arg0, arg1) {
        const ret = getObject(arg0).then(getObject(arg1));
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_toJSON_8a16b6cd55a62253 = function(arg0) {
        const ret = getObject(arg0).toJSON();
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_toString_14b47ee7542a49ef = function(arg0) {
        const ret = getObject(arg0).toString();
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_top_7d5b82a2c5d7f13f = function(arg0) {
        const ret = getObject(arg0).top;
        return ret;
    };
    imports.wbg.__wbg_transaction_754344c3ae25fdcf = function() { return handleError(function (arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).transaction(v0);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_transaction_790ec170b8fbc74b = function() { return handleError(function (arg0, arg1, arg2, arg3) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).transaction(v0, __wbindgen_enum_IdbTransactionMode[arg3]);
        return addHeapObject(ret);
    }, arguments) };
    imports.wbg.__wbg_value_2c75ca481407d038 = function(arg0, arg1) {
        const ret = getObject(arg1).value;
        const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    };
    imports.wbg.__wbg_value_57b7b035e117f7ee = function(arg0) {
        const ret = getObject(arg0).value;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_value_5ea6e5ab9f609290 = function(arg0, arg1) {
        const ret = getObject(arg1).value;
        const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len1 = WASM_VECTOR_LEN;
        getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
        getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
    };
    imports.wbg.__wbg_versions_c01dfd4722a88165 = function(arg0) {
        const ret = getObject(arg0).versions;
        return addHeapObject(ret);
    };
    imports.wbg.__wbg_warn_6e567d0d926ff881 = function(arg0) {
        console.warn(getObject(arg0));
    };
    imports.wbg.__wbg_writeText_c9776abb6826901c = function(arg0, arg1, arg2) {
        var v0 = getCachedStringFromWasm0(arg1, arg2);
        const ret = getObject(arg0).writeText(v0);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_0038a9097e15c9c8 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 7, function: Function { arguments: [NamedExternref("AudioProcessingEvent")], shim_idx: 8, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_337, __wasm_bindgen_func_elem_1795);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_01da35c289aa65d0 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 7, function: Function { arguments: [NamedExternref("MIDIMessageEvent")], shim_idx: 8, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_337, __wasm_bindgen_func_elem_1795);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_089570e11d94d589 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 7, function: Function { arguments: [NamedExternref("Array<any>")], shim_idx: 10, ret: Unit, inner_ret: Some(Unit) }, mutable: false }) -> Externref`.
        const ret = makeClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_337, __wasm_bindgen_func_elem_1794);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_1f2420152fb9448e = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 7, function: Function { arguments: [NamedExternref("PointerEvent")], shim_idx: 10, ret: Unit, inner_ret: Some(Unit) }, mutable: false }) -> Externref`.
        const ret = makeClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_337, __wasm_bindgen_func_elem_1794);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_25a1d7d60c12b6bc = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 858, function: Function { arguments: [NamedExternref("MessageEvent")], shim_idx: 859, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_4791, __wasm_bindgen_func_elem_5003);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_4f9b666b794767b7 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 858, function: Function { arguments: [NamedExternref("Event")], shim_idx: 859, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_4791, __wasm_bindgen_func_elem_5003);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_50dec378ce79c9a6 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 746, function: Function { arguments: [NamedExternref("Event")], shim_idx: 747, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_4356, __wasm_bindgen_func_elem_4380);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_726d936f3eb259b2 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 1812, function: Function { arguments: [Externref], shim_idx: 1813, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_10004, __wasm_bindgen_func_elem_10019);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_732c237fc346b069 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 1618, function: Function { arguments: [], shim_idx: 1619, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_8768, __wasm_bindgen_func_elem_8784);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_754ed5b648ed3028 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 1061, function: Function { arguments: [], shim_idx: 1062, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_5465, __wasm_bindgen_func_elem_5482);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_7e9c58eeb11b0a6f = function(arg0, arg1) {
        var v0 = getCachedStringFromWasm0(arg0, arg1);
        // Cast intrinsic for `Ref(CachedString) -> Externref`.
        const ret = v0;
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_8af563a582327ad8 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 7, function: Function { arguments: [NamedExternref("IDBVersionChangeEvent")], shim_idx: 8, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_337, __wasm_bindgen_func_elem_1795);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_8be2d9620109264a = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 1803, function: Function { arguments: [Ref(NamedExternref("Event"))], shim_idx: 1804, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_9881, __wasm_bindgen_func_elem_9894);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_96414fbd10ab6b75 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 7, function: Function { arguments: [NamedExternref("DragEvent")], shim_idx: 10, ret: Unit, inner_ret: Some(Unit) }, mutable: false }) -> Externref`.
        const ret = makeClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_337, __wasm_bindgen_func_elem_1794);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_a39ea65a15d7444f = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 7, function: Function { arguments: [], shim_idx: 9, ret: Unit, inner_ret: Some(Unit) }, mutable: false }) -> Externref`.
        const ret = makeClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_337, __wasm_bindgen_func_elem_1793);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_b6a7cb6aab9a53d6 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 858, function: Function { arguments: [NamedExternref("RTCPeerConnectionIceEvent")], shim_idx: 859, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_4791, __wasm_bindgen_func_elem_5003);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_cb9088102bce6b30 = function(arg0, arg1) {
        // Cast intrinsic for `Ref(Slice(U8)) -> NamedExternref("Uint8Array")`.
        const ret = getArrayU8FromWasm0(arg0, arg1);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_d02054f476282d87 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 746, function: Function { arguments: [NamedExternref("MessageEvent")], shim_idx: 747, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_4356, __wasm_bindgen_func_elem_4380);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_d6cd19b81560fd6e = function(arg0) {
        // Cast intrinsic for `F64 -> Externref`.
        const ret = arg0;
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_de412e7c1c6ee8a5 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 746, function: Function { arguments: [NamedExternref("CloseEvent")], shim_idx: 747, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_4356, __wasm_bindgen_func_elem_4380);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_df368b47a76e26fa = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 858, function: Function { arguments: [NamedExternref("RTCDataChannelEvent")], shim_idx: 859, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
        const ret = makeMutClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_4791, __wasm_bindgen_func_elem_5003);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_cast_e8383c7c69712977 = function(arg0, arg1) {
        // Cast intrinsic for `Closure(Closure { dtor_idx: 7, function: Function { arguments: [NamedExternref("Event")], shim_idx: 10, ret: Unit, inner_ret: Some(Unit) }, mutable: false }) -> Externref`.
        const ret = makeClosure(arg0, arg1, wasm.__wasm_bindgen_func_elem_337, __wasm_bindgen_func_elem_1794);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_object_clone_ref = function(arg0) {
        const ret = getObject(arg0);
        return addHeapObject(ret);
    };
    imports.wbg.__wbindgen_object_drop_ref = function(arg0) {
        takeObject(arg0);
    };
    imports['./snippets/walkie-songie-54ae4c155955ba2c/assets/onnx-bridge.js'] = __wbg_star0;

    return imports;
}

function __wbg_finalize_init(instance, module) {
    wasm = instance.exports;
    __wbg_init.__wbindgen_wasm_module = module;
    cachedDataViewMemory0 = null;
    cachedFloat32ArrayMemory0 = null;
    cachedUint8ArrayMemory0 = null;


    wasm.__wbindgen_start();
    return wasm;
}

function initSync(module) {
    if (wasm !== undefined) return wasm;


    if (typeof module !== 'undefined') {
        if (Object.getPrototypeOf(module) === Object.prototype) {
            ({module} = module)
        } else {
            console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
        }
    }

    const imports = __wbg_get_imports();
    if (!(module instanceof WebAssembly.Module)) {
        module = new WebAssembly.Module(module);
    }
    const instance = new WebAssembly.Instance(module, imports);
    return __wbg_finalize_init(instance, module);
}

async function __wbg_init(module_or_path) {
    if (wasm !== undefined) return wasm;


    if (typeof module_or_path !== 'undefined') {
        if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
            ({module_or_path} = module_or_path)
        } else {
            console.warn('using deprecated parameters for the initialization function; pass a single object instead')
        }
    }

    if (typeof module_or_path === 'undefined') {
        module_or_path = new URL('walkie_songie_bg.wasm', import.meta.url);
    }
    const imports = __wbg_get_imports();

    if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
        module_or_path = fetch(module_or_path);
    }

    const { instance, module } = await __wbg_load(await module_or_path, imports);

    return __wbg_finalize_init(instance, module);
}

export { initSync };
export default __wbg_init;
