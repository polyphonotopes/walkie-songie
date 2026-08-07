/* tslint:disable */
/* eslint-disable */

/**
 * Run the web application.
 */
export function run_app(): void;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
  readonly memory: WebAssembly.Memory;
  readonly run_app: () => void;
  readonly __wasm_bindgen_func_elem_5003: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_4791: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_1795: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_337: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_1793: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_1794: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_4380: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_4356: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_5482: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_5465: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_8784: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_8768: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_10019: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_10004: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_9894: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_9881: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_13511: (a: number, b: number, c: number, d: number) => void;
  readonly __wbindgen_export: (a: number, b: number) => number;
  readonly __wbindgen_export2: (a: number, b: number, c: number, d: number) => number;
  readonly __wbindgen_export3: (a: number) => void;
  readonly __wbindgen_export4: (a: number, b: number, c: number) => void;
  readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
* Instantiates the given `module`, which can either be bytes or
* a precompiled `WebAssembly.Module`.
*
* @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
*
* @returns {InitOutput}
*/
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
* If `module_or_path` is {RequestInfo} or {URL}, makes a request and
* for everything else, calls `WebAssembly.instantiate` directly.
*
* @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
*
* @returns {Promise<InitOutput>}
*/
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
