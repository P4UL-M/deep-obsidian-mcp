/**
 * A minimal CouchDB emulator: just enough of the wire protocol for PouchDB's
 * HTTP adapter, which is what commonlib talks to.
 *
 * Why an HTTP server rather than an injected `fetch` mock: the sidecar under
 * test is a *child process*, so nothing can be injected into it. Testing over
 * real sockets is also the point -- it exercises framing, the transport, and
 * PouchDB's actual request shapes rather than a hand-written idea of them.
 *
 * The endpoint set was discovered empirically (point the sidecar at this server
 * with `DEBUG_REQUESTS=1` and read stderr), not derived from documentation.
 * Anything unhandled answers 501 and is recorded, so a new upstream request
 * shape fails loudly instead of silently degrading.
 *
 * Documents are stored in the real LiveSync layout: entry documents whose `_id`
 * is the lower-cased path (upstream lower-cases ids unless
 * `handleFilenameCaseSensitive` is set), `h:`-prefixed `leaf` chunk documents,
 * the `obsydian_livesync_version` schema document, and `_local/` control
 * documents.
 */
import http from "node:http";

const JSON_HEADERS = { "content-type": "application/json" };

/** Sorts like CouchDB: by raw code unit, which is what the id ranges rely on. */
function byId(a, b) {
    return a < b ? -1 : a > b ? 1 : 0;
}

function parseBool(value) {
    return value === "true" || value === true;
}

/**
 * CouchDB's `_all_docs` takes JSON-encoded keys (`startkey=%22a%22`). Bare
 * strings are tolerated too because it costs nothing and makes the mock easier
 * to drive by hand.
 */
function parseKey(value) {
    if (value === null || value === undefined) return undefined;
    try {
        return JSON.parse(value);
    } catch {
        return value;
    }
}

export class MockCouch {
    /**
     * @param {object} options
     * @param {string} [options.dbName]
     * @param {Record<string, object>} [options.docs] id -> document body
     * @param {Record<string, object>} [options.localDocs] id (with `_local/`) -> body
     * @param {Record<string, string[]>} [options.conflicts] id -> extra conflict revs
     * @param {number} [options.authStatus] when set, every request answers with it
     */
    constructor(options = {}) {
        this.dbName = options.dbName ?? "vault";
        this.docs = new Map();
        this.localDocs = new Map();
        this.conflicts = new Map(Object.entries(options.conflicts ?? {}));
        this.authStatus = options.authStatus;
        this.changes = [];
        this.seq = 0;
        /** Every request, for assertions (e.g. `GET /vault/_all_docs?...`). */
        this.requests = [];
        /** Requests that would modify the remote. Must stay empty. */
        this.writes = [];
        /** Requests answered with 501 because this mock does not know them. */
        this.unhandled = [];
        this.debug = process.env.DEBUG_REQUESTS === "1";
        /** Pending longpoll/continuous releases, triggered by `pushChange`. */
        this.waiters = [];
        this.lastStreamed = "0";

        for (const [id, body] of Object.entries(options.docs ?? {})) {
            this.putDoc(id, body);
        }
        for (const [id, body] of Object.entries(options.localDocs ?? {})) {
            this.localDocs.set(id, { _id: id, _rev: "0-1", ...body });
        }
    }

    putDoc(id, body) {
        const rev = body._rev ?? `1-${Buffer.from(id).toString("hex").slice(0, 8).padEnd(8, "0")}`;
        const doc = { ...body, _id: id, _rev: rev };
        this.docs.set(id, doc);
        this.seq += 1;
        this.changes.push({ seq: this.seq, id, rev, deleted: Boolean(body._deleted) });
        return doc;
    }

    /** Adds a document and releases any held feed, as a live edit would. */
    pushChange(id, body) {
        const doc = this.putDoc(id, body);
        const waiters = this.waiters;
        this.waiters = [];
        for (const release of waiters) release();
        return doc;
    }

    async listen() {
        this.server = http.createServer((req, res) => {
            this.route(req, res).catch((error) => {
                process.stderr.write(`MOCK ERROR ${error?.stack ?? error}\n`);
                try {
                    send(res, 500, { error: "mock_failure", reason: String(error) });
                } catch {
                    /* response already sent */
                }
            });
        });
        // Otherwise a held connection keeps `node --test` from exiting.
        this.server.keepAliveTimeout = 1;
        await new Promise((resolve) => this.server.listen(0, "127.0.0.1", resolve));
        this.port = this.server.address().port;
        this.url = `http://127.0.0.1:${this.port}`;
        return this.url;
    }

    async close() {
        const waiters = this.waiters;
        this.waiters = [];
        for (const release of waiters) release();
        if (!this.server) return;
        this.server.closeAllConnections();
        await new Promise((resolve) => this.server.close(resolve));
        this.server = undefined;
    }

    async route(req, res) {
        const body = await readBody(req);
        const url = new URL(req.url, "http://localhost");
        const record = `${req.method} ${req.url}`;
        this.requests.push(record);
        if (this.debug) process.stderr.write(`MOCK ${record}${body ? ` ${body.slice(0, 300)}` : ""}\n`);

        if (this.authStatus !== undefined) {
            return send(res, this.authStatus, { error: "unauthorized", reason: "mock" });
        }

        const segments = url.pathname.split("/").filter((s) => s !== "");
        const params = url.searchParams;
        const parsedBody = body ? safeJson(body) : undefined;

        // Server root: PouchDB probes it for the CouchDB version.
        if (segments.length === 0) {
            return send(res, 200, { couchdb: "Welcome", version: "3.3.3", vendor: { name: "mock" } });
        }

        if (segments[0] !== this.dbName) {
            return send(res, 404, { error: "not_found", reason: "no_db_file" });
        }

        // Anything that is not a read is a bug in the sidecar, not a gap in the
        // mock: record it so a test can assert the read-only posture, and answer
        // 403 so the attempt visibly fails.
        if (req.method !== "GET" && req.method !== "POST" && req.method !== "HEAD") {
            this.writes.push(record);
            return send(res, 403, { error: "forbidden", reason: "read-only mock" });
        }
        if (req.method === "POST" && segments.length > 1) {
            const name = segments[1];
            if (name === "_bulk_docs" || name === "_revs_diff" || name === "_compact" || name === "_purge") {
                this.writes.push(record);
                return send(res, 403, { error: "forbidden", reason: "read-only mock" });
            }
        }

        // GET /{db} -- database info.
        if (segments.length === 1) {
            if (req.method === "HEAD") return send(res, 200, {});
            return send(res, 200, {
                db_name: this.dbName,
                doc_count: this.docs.size,
                doc_del_count: 0,
                update_seq: String(this.seq),
                purge_seq: 0,
                compact_running: false,
                disk_format_version: 8,
                instance_start_time: "0",
                sizes: { active: 1, external: 1, file: 1 },
            });
        }

        const endpoint = segments[1];

        if (endpoint === "_all_docs") {
            return send(res, 200, this.allDocs(params, parsedBody));
        }
        if (endpoint === "_changes") {
            return await this.changesFeed(res, params, parsedBody);
        }
        if (endpoint === "_bulk_get") {
            return send(res, 200, this.bulkGet(parsedBody));
        }
        if (endpoint === "_local") {
            const id = `_local/${segments.slice(2).join("/")}`;
            const doc = this.localDocs.get(id);
            if (!doc) return send(res, 404, { error: "not_found", reason: "missing" });
            return send(res, 200, doc);
        }
        if (endpoint === "_design") {
            // No design documents exist in a real LiveSync vault either; this is
            // exactly what makes upstream's `replicate/pull` filter 404.
            return send(res, 404, { error: "not_found", reason: "missing" });
        }
        if (endpoint === "_revs_limit" || endpoint === "_security" || endpoint === "_ensure_full_commit") {
            return send(res, 200, {});
        }
        if (endpoint.startsWith("_")) {
            this.unhandled.push(record);
            return send(res, 501, { error: "not_implemented", reason: `mock lacks ${endpoint}` });
        }

        // GET /{db}/{docid} -- ids may contain slashes, hence the rejoin.
        const docId = decodeURIComponent(segments.slice(1).join("/"));
        const [status, payload] = this.getDoc(docId, params);
        return send(res, status, payload);
    }

    getDoc(id, params) {
        const doc = this.docs.get(id);
        if (!doc) return [404, { error: "not_found", reason: "missing" }];
        const out = { ...doc };
        if (parseBool(params.get("conflicts"))) {
            const conflicts = this.conflicts.get(id);
            if (conflicts?.length) out._conflicts = [...conflicts];
        }
        if (parseBool(params.get("revs_info"))) {
            out._revs_info = [{ rev: doc._rev, status: "available" }];
        }
        if (parseBool(params.get("revs"))) {
            const [num, hash] = String(doc._rev).split("-");
            out._revisions = { start: Number(num), ids: [hash] };
        }
        return [200, out];
    }

    allDocs(params, body) {
        const includeDocs = parseBool(params.get("include_docs")) || Boolean(body?.include_docs);
        const withConflicts = parseBool(params.get("conflicts")) || Boolean(body?.conflicts);
        const limit = params.get("limit") !== null ? Number(params.get("limit")) : body?.limit;
        const skip = Number(params.get("skip") ?? body?.skip ?? 0) || 0;
        const keys = body?.keys ?? parseKey(params.get("keys"));
        const singleKey = body?.key ?? parseKey(params.get("key"));
        const startkey = body?.startkey ?? parseKey(params.get("startkey"));
        const endkey = body?.endkey ?? parseKey(params.get("endkey"));
        const inclusiveEndRaw = params.get("inclusive_end") ?? body?.inclusive_end;
        const inclusiveEnd =
            inclusiveEndRaw === undefined || inclusiveEndRaw === null ? true : parseBool(inclusiveEndRaw);

        if (Array.isArray(keys)) {
            // Explicit keys preserve request order and report misses.
            const rows = keys.map((key) => {
                const doc = this.docs.get(key);
                if (!doc) return { key, error: "not_found" };
                return this.row(doc, includeDocs, withConflicts);
            });
            return { total_rows: this.docs.size, offset: 0, rows };
        }

        let ids;
        if (singleKey !== undefined) {
            ids = this.docs.has(singleKey) ? [singleKey] : [];
        } else {
            ids = [...this.docs.keys()].sort(byId);
            if (startkey !== undefined) ids = ids.filter((id) => id >= startkey);
            if (endkey !== undefined) {
                ids = ids.filter((id) => (inclusiveEnd ? id <= endkey : id < endkey));
            }
        }
        if (skip) ids = ids.slice(skip);
        if (limit !== undefined && limit !== null && Number.isFinite(limit)) ids = ids.slice(0, limit);
        return {
            total_rows: this.docs.size,
            offset: skip,
            rows: ids.map((id) => this.row(this.docs.get(id), includeDocs, withConflicts)),
        };
    }

    row(doc, includeDocs, withConflicts) {
        const row = { id: doc._id, key: doc._id, value: { rev: doc._rev } };
        if (includeDocs) {
            row.doc = { ...doc };
            if (withConflicts) {
                const conflicts = this.conflicts.get(doc._id);
                if (conflicts?.length) row.doc._conflicts = [...conflicts];
            }
        }
        return row;
    }

    bulkGet(body) {
        const requested = body?.docs ?? [];
        return {
            results: requested.map(({ id }) => {
                const doc = this.docs.get(id);
                if (!doc) {
                    return { id, docs: [{ error: { id, error: "not_found", reason: "missing" } }] };
                }
                return { id, docs: [{ ok: { ...doc } }] };
            }),
        };
    }

    /**
     * `_changes` in three modes: normal (answer immediately), longpoll (hold
     * until there is something or the timeout expires), and continuous (stream
     * newline-delimited rows). commonlib opens a live feed while initialising
     * the database, so longpoll/continuous support is required for `initialize`
     * to work at all -- not only for `watch`.
     */
    async changesFeed(res, params, body) {
        const feed = params.get("feed") ?? "normal";
        const since = params.get("since") ?? body?.since ?? "0";
        const includeDocs = parseBool(params.get("include_docs")) || Boolean(body?.include_docs);
        const limitRaw = params.get("limit");
        const limit = limitRaw !== null ? Number(limitRaw) : undefined;
        const selector = body?.selector;

        /**
         * Rows after `from`, plus the `last_seq` CouchDB would report.
         *
         * `last_seq` is the seq of the *last row returned* when `limit`
         * truncated the feed, and the database's current seq otherwise. Getting
         * this wrong is not cosmetic: PouchDB pages a `_changes` request into
         * batches (25 by default) and re-requests from the `last_seq` it was
         * given, so a mock that always reports the maximum seq makes the client
         * skip everything past the first batch -- while looking like it drained
         * the feed.
         */
        const collect = (from, applyLimit = true) => {
            let rows = this.changes
                .filter((change) => change.seq > Number(from))
                .map((change) => {
                    const doc = this.docs.get(change.id);
                    const row = { seq: String(change.seq), id: change.id, changes: [{ rev: change.rev }] };
                    if (change.deleted) row.deleted = true;
                    if (includeDocs && doc) row.doc = { ...doc };
                    return row;
                });
            if (selector) rows = rows.filter((row) => matchSelector(row.doc, selector));
            let truncated = false;
            if (applyLimit && limit !== undefined && Number.isFinite(limit) && rows.length > limit) {
                rows = rows.slice(0, limit);
                truncated = true;
            }
            const lastSeq = truncated && rows.length > 0 ? rows[rows.length - 1].seq : String(this.seq);
            return { results: rows, last_seq: lastSeq };
        };

        if (feed === "continuous") {
            res.writeHead(200, { "content-type": "application/json" });
            let sent = Number(since);
            const flush = () => {
                for (const row of collect(sent, false).results) {
                    res.write(`${JSON.stringify(row)}\n`);
                }
                sent = this.seq;
                this.waiters.push(flush);
            };
            flush();
            return;
        }

        if (feed === "longpoll") {
            const immediate = collect(since);
            if (immediate.results.length > 0) {
                return send(res, 200, immediate);
            }
            // Capped well below CouchDB's default so a hung feed cannot stall a
            // test run; the sidecar re-polls, which is the real behaviour anyway.
            const timeoutMs = Math.min(Number(params.get("timeout") ?? 25000) || 25000, 2000);
            await new Promise((resolve) => {
                let settled = false;
                const finish = () => {
                    if (settled) return;
                    settled = true;
                    clearTimeout(timer);
                    resolve();
                };
                const timer = setTimeout(finish, timeoutMs);
                timer.unref?.();
                this.waiters.push(finish);
            });
            return send(res, 200, collect(since));
        }

        return send(res, 200, { ...collect(since), pending: 0 });
    }
}

/** Tiny subset of Mango: enough for `{type: {$ne: "leaf"}}`, all upstream sends. */
function matchSelector(doc, selector) {
    if (!doc) return true;
    for (const [field, condition] of Object.entries(selector)) {
        const value = doc[field];
        if (condition && typeof condition === "object") {
            if ("$ne" in condition && value === condition.$ne) return false;
            if ("$eq" in condition && value !== condition.$eq) return false;
            if ("$gt" in condition && !(value > condition.$gt)) return false;
            if ("$lt" in condition && !(value < condition.$lt)) return false;
        } else if (value !== condition) {
            return false;
        }
    }
    return true;
}

function send(res, status, payload) {
    const text = JSON.stringify(payload);
    res.writeHead(status, { ...JSON_HEADERS, "content-length": Buffer.byteLength(text) });
    res.end(text);
}

function safeJson(text) {
    try {
        return JSON.parse(text);
    } catch {
        return undefined;
    }
}

function readBody(req) {
    return new Promise((resolve) => {
        let data = "";
        req.on("data", (chunk) => (data += chunk));
        req.on("end", () => resolve(data));
    });
}
