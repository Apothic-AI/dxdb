/**
 * Interop: a real upstream RxDB client against a dxdb replication endpoint.
 *
 * This is the claim PLAN.md section 10 makes and nothing has yet proved: a Rust port that
 * only talks to itself is not interop. The upstream side here is genuine — real
 * rxdb@17.6.0 at the pin, bundled from source by `vendor.mjs`, running its own
 * `replicateRxCollection` and its own `replication-websocket` client.
 *
 * # Two directions, tested separately
 *
 * 1. **Upstream client -> dxdb server.** Upstream's `replicateWithWebsocketServer` dials a
 *    dxdb endpoint. This is the commercially meaningful direction: an existing JS app
 *    replicating against a Rust backend. Implemented in `client-upstream.mjs`.
 * 2. **dxdb client -> upstream server.** A dxdb client dials upstream's
 *    `startWebsocketServer`. This proves the other half and, because upstream's server is
 *    the reference implementation of the endpoint, it is also the strongest statement
 *    about the frame format.
 *
 * `server-dxdb.mjs` stands in for the dxdb side in (1) by speaking the same frames the Rust
 * server emits, byte-for-byte, with the document payloads produced by the Rust code. That
 * is a **fixture**, and the file says so loudly: the Rust server cannot be driven from Node
 * directly, so what is being tested in (1) is our *wire format*, not our Rust process.
 *
 * # What is asserted
 *
 * - documents written by each side appear on the other;
 * - the checkpoint each side persists for the same replication agrees;
 * - after both sides restart, nothing is re-sent.
 *
 * # Why this file is not a Rust test
 *
 * It needs real upstream. The Rust suite runs in `cargo test`, which cannot install npm
 * packages. `xtask` drives this file and fails the build if it does not pass.
 */
import { loadUpstream } from './vendor.mjs';

export { loadUpstream };

/**
 * Starts a WebSocket server over a dxdb-shaped replication handler.
 *
 * The handler is supplied by the caller; this only owns the socket and the frame loop, and
 * its loop is copied from upstream's `startWebsocketServer`
 * (`websocket-server.ts:96-130`) so a divergence in *our* server would show up as a
 * difference here rather than being papered over.
 */
export async function startEndpoint({ port = 0, handler }) {
  const { WebSocketServer } = await import('ws');
  const server = new WebSocketServer({ port, host: '127.0.0.1' });
  await new Promise((resolve, reject) => {
    server.once('listening', resolve);
    server.once('error', reject);
  });
  const address = server.address();
  const frames = [];

  server.on('connection', (socket) => {
    socket.on('message', async (raw) => {
      let message;
      try {
        message = JSON.parse(String(raw));
      } catch {
        return;
      }
      frames.push({ direction: 'in', message });
      if (message.method === 'auth') {
        return;
      }
      // Upstream's server distinguishes a method from the change stream by `typeof method
      // === 'function'`; here the handler is a plain object with the same three members.
      if (message.method === 'masterChangeStream$') {
        const subscription = handler.masterChangeStream$.subscribe?.((event) => {
          socket.send(
            JSON.stringify({ id: 'stream', collection: message.collection, result: event })
          );
        });
        return;
      }
      const method = handler[message.method];
      if (typeof method !== 'function') {
        socket.send(
          JSON.stringify({
            id: message.id,
            collection: message.collection,
            result: { error: `unknown method ${message.method}` },
          })
        );
        return;
      }
      let result;
      try {
        result = await method(...message.params);
      } catch (error) {
        result = { error: String(error?.message ?? error) };
      }
      const response = { id: message.id, collection: message.collection, result };
      frames.push({ direction: 'out', message: response });
      socket.send(JSON.stringify(response));
    });
  });

  return {
    port: address.port,
    url: `ws://127.0.0.1:${address.port}`,
    frames,
    close: () =>
      new Promise((resolve) => {
        for (const client of server.clients) {
          client.close();
        }
        server.close(() => resolve());
      }),
  };
}
