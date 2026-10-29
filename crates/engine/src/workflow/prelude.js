// The workflow script API, layered on three host bindings:
// `__holt_agent(prompt, optsJson) -> Promise<resultJson>`, `__holt_log(text)`,
// and the run's `__holt_args` JSON. Everything orchestration-shaped
// (`parallel`, `pipeline`, phases) is plain JS here, so the host stays a
// single "run one child" primitive.
"use strict";

(() => {
  const MAX_ITEMS = 4096;
  const g = globalThis;

  // Replays must issue the same agent() calls: wall-clock time and
  // randomness would let a relaunched run diverge from its journal.
  const deny = (what) => () => {
    throw new Error(`${what} is unavailable in workflow scripts; pass values in through args`);
  };
  const RealDate = Date;
  function WorkflowDate(...a) {
    if (a.length === 0) deny("new Date() without arguments")();
    return new.target ? new RealDate(...a) : RealDate(...a);
  }
  WorkflowDate.prototype = RealDate.prototype;
  WorkflowDate.UTC = RealDate.UTC;
  WorkflowDate.parse = RealDate.parse;
  WorkflowDate.now = deny("Date.now()");
  g.Date = WorkflowDate;
  Math.random = deny("Math.random()");

  g.args = JSON.parse(g.__holt_args);

  g.phase = () => {
    throw new Error("phase() is gone: declare steps in the Workflow plan and pass agent(prompt, { step })");
  };

  g.log = (...parts) => {
    g.__holt_log(parts.map((p) => (typeof p === "string" ? p : JSON.stringify(p))).join(" "));
  };

  // An option the host does not read would be dropped silently — a
  // misspelled `schema` then runs every child unstructured.
  const AGENT_OPTIONS = ["step", "label", "role", "schema"];
  const OPTION_HINTS = { output_schema: "schema", phase: "step" };

  g.agent = async (prompt, opts = {}) => {
    for (const key of Object.keys(opts)) {
      if (AGENT_OPTIONS.includes(key)) continue;
      const hint = OPTION_HINTS[key] ? ` (did you mean ${OPTION_HINTS[key]}?)` : "";
      throw new Error(
        `agent() has no option ${JSON.stringify(key)}${hint}; it takes ${AGENT_OPTIONS.join(", ")}`,
      );
    }
    const options = {
      label: opts.label == null ? null : String(opts.label),
      step: opts.step == null ? null : String(opts.step),
      role: opts.role == null ? "explorer" : String(opts.role),
      schema: opts.schema == null ? null : opts.schema,
    };
    if (options.role !== "explorer" && options.role !== "worker") {
      throw new Error(`agent() role must be "explorer" or "worker", not ${JSON.stringify(options.role)}`);
    }
    if (options.schema !== null && (typeof options.schema !== "object" || options.schema.type !== "object")) {
      throw new Error('agent() schema must be a JSON Schema with type "object"');
    }
    const raw = await g.__holt_agent(String(prompt), JSON.stringify(options));
    return JSON.parse(raw);
  };

  const bounded = (list, what) => {
    if (!Array.isArray(list)) throw new Error(`${what} expects an array`);
    if (list.length > MAX_ITEMS) {
      throw new Error(`${what} takes at most ${MAX_ITEMS} items; got ${list.length}`);
    }
  };
  const settle = (promise) =>
    Promise.resolve(promise).catch((error) => {
      g.log(`step failed: ${error && error.message ? error.message : error}`);
      return null;
    });

  // Every thunk starts now, in array order; one failing yields null in its
  // slot instead of rejecting the whole set.
  g.parallel = (thunks) => {
    bounded(thunks, "parallel()");
    return Promise.all(thunks.map((thunk) => settle(Promise.resolve().then(thunk))));
  };

  // Each item walks the stages on its own: item 1's second stage never
  // waits for item 0's first. A stage sees (previous, item, index); a null
  // (failed) previous result stops that item.
  g.pipeline = (items, ...stages) => {
    bounded(items, "pipeline()");
    return Promise.all(
      items.map((item, index) =>
        settle(
          stages.reduce(
            (previous, stage) =>
              previous.then((value) => (value === null ? null : stage(value, item, index))),
            Promise.resolve(item),
          ),
        ),
      ),
    );
  };
})();
