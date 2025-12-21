// Bridge to onnxruntime-web for SwiftF0 pitch detection
import * as ort from 'https://cdn.jsdelivr.net/npm/onnxruntime-web@1.21.0/dist/ort.webgpu.min.mjs';

let session = null;
let modelReady = false;

// Initialize the SwiftF0 model
export async function initSwiftF0() {
    try {
        // Use wasm backend (CPU, works everywhere)
        ort.env.wasm.wasmPaths = 'https://cdn.jsdelivr.net/npm/onnxruntime-web@1.21.0/dist/';

        // Fetch the model as ArrayBuffer - trunk copies it to root
        const modelUrl = '/swiftf0.onnx';
        console.log('Fetching SwiftF0 model from:', modelUrl);

        const response = await fetch(modelUrl);
        if (!response.ok) {
            throw new Error(`Failed to fetch model: ${response.status} ${response.statusText}`);
        }
        const modelBuffer = await response.arrayBuffer();
        console.log('Model fetched, size:', modelBuffer.byteLength, 'bytes');

        // Load from ArrayBuffer
        session = await ort.InferenceSession.create(modelBuffer, {
            executionProviders: ['wasm'],
        });

        modelReady = true;
        console.log('SwiftF0 ONNX model loaded via onnxruntime-web');
    } catch (e) {
        console.error('Failed to load SwiftF0 model:', e);
        throw e;
    }
}

// Check if model is ready
export function isModelReady() {
    return modelReady;
}

// Detect pitch from audio samples (16kHz expected)
export async function detectPitch(samples) {
    if (!session || !modelReady) {
        return null;
    }

    try {
        // Create input tensor [1, samples.length]
        const inputTensor = new ort.Tensor('float32', samples, [1, samples.length]);

        // Run inference
        const results = await session.run({ input_audio: inputTensor });

        // Get outputs
        const pitchHz = results.pitch_hz?.data || results[Object.keys(results)[0]]?.data;
        const confidence = results.confidence?.data || results[Object.keys(results)[1]]?.data;

        if (!pitchHz || !confidence || pitchHz.length === 0) {
            return null;
        }

        // Return last frame's pitch and confidence
        const lastIdx = pitchHz.length - 1;
        return [pitchHz[lastIdx], confidence[lastIdx]];
    } catch (e) {
        console.error('Pitch detection error:', e);
        return null;
    }
}
