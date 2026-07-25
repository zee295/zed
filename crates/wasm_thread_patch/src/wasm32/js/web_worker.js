// synchronously, using the browser, import wasm_bindgen shim JS scripts
importScripts('WASM_BINDGEN_SHIM_URL');

async function prepareSqlRpcBridge() {
    if (self.name !== "sqlez-worker") return;

    const source = `
        let socket;
        let endpoint;
        let nextId = 1;
        const waiting = new Map();
        const sendQueue = [];
        const encoder = new TextEncoder();
        const decoder = new TextDecoder();
        let control;
        let requestData;
        let responseData;

        function finish(target, envelope, state = 1) {
            const bytes = encoder.encode(JSON.stringify(envelope));
            const length = Math.min(bytes.length, responseData.length);
            responseData.set(bytes.subarray(0, length));
            Atomics.store(control, 3, length);
            Atomics.store(control, 4, bytes.length > responseData.length ? -2 : state);
            Atomics.store(control, 1, target.sequence);
            Atomics.notify(control, 1);
        }

        function ensureSocket(url) {
            if (socket && endpoint === url && socket.readyState <= WebSocket.OPEN) return;
            endpoint = url;
            socket = new WebSocket(url);
            socket.onopen = () => {
                while (sendQueue.length) socket.send(sendQueue.shift());
            };
            socket.onmessage = event => {
                const message = JSON.parse(event.data);
                if (message.id == null) return;
                const target = waiting.get(message.id);
                if (!target) return;
                waiting.delete(message.id);
                finish(target, { result: message.result, error: message.error || null });
            };
            socket.onerror = () => {
                for (const target of waiting.values()) {
                    finish(target, { error: "SQL RPC WebSocket failed" }, -1);
                }
                waiting.clear();
            };
        }

        async function requestLoop() {
            let sequence = Atomics.load(control, 0);
            for (;;) {
                const current = Atomics.load(control, 0);
                if (current === sequence) {
                    const waiter = Atomics.waitAsync(control, 0, sequence);
                    if (waiter.async) await waiter.value;
                    else await Promise.resolve();
                    continue;
                }
                sequence = current;
                const length = Atomics.load(control, 2);
                const request = JSON.parse(decoder.decode(requestData.slice(0, length)));
                ensureSocket(request.endpoint);
                delete request.endpoint;
                const id = nextId++;
                request.id = id;
                waiting.set(id, { sequence });
                const encoded = JSON.stringify(request);
                if (socket.readyState === WebSocket.OPEN) socket.send(encoded);
                else sendQueue.push(encoded);
            }
        }

        self.onmessage = event => {
            if (event.data?.type !== "init") return;
            control = new Int32Array(event.data.control);
            requestData = new Uint8Array(event.data.requestData);
            responseData = new Uint8Array(event.data.responseData);
            postMessage({ ready: true });
            requestLoop();
        };
    `;
    const url = URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
    const worker = new Worker(url);
    const buffers = {
        control: new SharedArrayBuffer(32),
        requestData: new SharedArrayBuffer(8 * 1024 * 1024),
        responseData: new SharedArrayBuffer(8 * 1024 * 1024),
    };
    worker.postMessage({ type: "init", ...buffers });
    await new Promise((resolve, reject) => {
        worker.onmessage = event => event.data?.ready && resolve();
        worker.onerror = reject;
    });
    URL.revokeObjectURL(url);
    self.__zedSqlRpcBridge = worker;
    self.__zedSqlRpcBuffers = buffers;
    console.log("sqlez RPC network worker ready");
}

// Wait for the main thread to send us the shared module/memory and work context.
// Once we've got it, initialize it all with the `wasm_bindgen` global we imported via
// `importScripts`.
self.onmessage = event => {
    let [ module, memory, work ] = event.data;

    Promise.all([wasm_bindgen(module, memory), prepareSqlRpcBridge()]).catch(err => {
        console.log(err);

        // Propagate to main `onerror`:
        setTimeout(() => {
            throw err;
        });
        // Rethrow to keep promise rejected and prevent execution of further commands:
        throw err;
    }).then(([wasm]) => {
        // Enter rust code by calling entry point defined in `lib.rs`.
        // This executes closure defined by work context.
        wasm.wasm_thread_entry_point(work);

        // Once done, terminate web worker
        close();
    });
};
  
