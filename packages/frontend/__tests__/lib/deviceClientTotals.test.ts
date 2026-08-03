import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { PgDialect } from "drizzle-orm/pg-core";
import type { SQL } from "drizzle-orm";

import {
  DEVICE_CLIENT_TOTALS_BUCKET_WIDTH,
  DEVICE_CLIENT_TOTALS_WRITE_FLAG,
  buildDualDerivationRecord,
  foldContributionsIntoBuckets,
  interpretHighwaterAggregate,
  isDeviceClientTotalsWriteEnabled,
  monthBucketKey,
  readHighwaterTotal,
  recordDeviceClientTotals,
  type DeviceClientTotalsContribution,
} from "@/lib/db/deviceClientTotals";

// Pins Phase 1 / Phase 1.5 of docs/ratchet-inflation-recovery.md:
// per-device, per-client, per-bucket token/cost HIGH-WATER marks that nothing
// reads, written after the submit transaction commits.

const dialect = new PgDialect();

interface CapturedQuery {
  sql: string;
  params: unknown[];
}

function capturingExecutor() {
  const queries: CapturedQuery[] = [];
  return {
    queries,
    execute: async (query: SQL) => {
      queries.push(dialect.sqlToQuery(query));
      return [];
    },
  };
}

function day(
  date: string,
  clients: Array<{ client: string; modelId?: string; tokens?: number; cost?: number }>
): DeviceClientTotalsContribution {
  return {
    date,
    clients: clients.map((c) => ({
      client: c.client,
      modelId: c.modelId ?? "model-a",
      messages: 1,
      cost: c.cost ?? 0,
      tokens: {
        input: c.tokens ?? 0,
        output: 0,
        cacheRead: 0,
        cacheWrite: 0,
        reasoning: 0,
      },
    })),
  };
}

describe("monthBucketKey", () => {
  it("folds an ISO date to its month", () => {
    expect(monthBucketKey("2026-05-11")).toBe("2026-05");
    expect(monthBucketKey("2026-12-31")).toBe("2026-12");
  });

  it("refuses to invent a bucket from a malformed date", () => {
    expect(monthBucketKey("2026-5-11")).toBeNull();
    expect(monthBucketKey("not-a-date")).toBeNull();
    expect(monthBucketKey("2026-13-01")).toBeNull();
    expect(monthBucketKey("")).toBeNull();
  });
});

describe("foldContributionsIntoBuckets", () => {
  it("sums every day of a month into one (client, bucket) row", () => {
    const buckets = foldContributionsIntoBuckets(
      [
        day("2026-05-01", [{ client: "codex", tokens: 10, cost: 1 }]),
        day("2026-05-31", [{ client: "codex", tokens: 5, cost: 0.5 }]),
        day("2026-06-01", [{ client: "codex", tokens: 100, cost: 2 }]),
      ],
      "cli"
    );

    expect(buckets).toEqual([
      {
        client: "codex",
        origin: "cli",
        bucketWidth: "month",
        bucketKey: "2026-05",
        tokens: 15,
        cost: 1.5,
      },
      {
        client: "codex",
        origin: "cli",
        bucketWidth: "month",
        bucketKey: "2026-06",
        tokens: 100,
        cost: 2,
      },
    ]);
  });

  it("keeps distinct clients in distinct buckets", () => {
    const buckets = foldContributionsIntoBuckets(
      [
        day("2026-05-01", [
          { client: "codex", tokens: 10 },
          { client: "claude-code", tokens: 7 },
        ]),
      ],
      "cli"
    );

    expect(buckets.map((b) => [b.client, b.tokens])).toEqual([
      ["codex", 10],
      ["claude-code", 7],
    ]);
  });

  it("writes only the month width, halving the per-submit row count", () => {
    const buckets = foldContributionsIntoBuckets(
      [day("2026-05-01", [{ client: "codex", tokens: 1 }])],
      "cli"
    );
    expect(buckets).toHaveLength(1);
    expect(new Set(buckets.map((b) => b.bucketWidth))).toEqual(
      new Set([DEVICE_CLIENT_TOTALS_BUCKET_WIDTH])
    );
  });

  it("sums every token class, matching the daily-row derivation", () => {
    const buckets = foldContributionsIntoBuckets(
      [
        {
          date: "2026-05-01",
          clients: [
            {
              client: "codex",
              modelId: "m",
              messages: 1,
              cost: 3,
              tokens: {
                input: 1,
                output: 2,
                cacheRead: 4,
                cacheWrite: 8,
                reasoning: 16,
              },
            },
          ],
        },
      ],
      "cli"
    );
    expect(buckets[0].tokens).toBe(31);
  });

  it("emits ONE row per (client, bucket) even when a day repeats a client", () => {
    // A day legitimately carries one entry per (client, model). If those did
    // not fold, a single INSERT would carry two rows with the same primary key
    // and Postgres would abort it with "ON CONFLICT DO UPDATE command cannot
    // affect row a second time" — the same hazard the duplicate-dates refine()
    // on the submission schema exists to prevent.
    const buckets = foldContributionsIntoBuckets(
      [
        day("2026-05-01", [
          { client: "codex", modelId: "gpt-5.5", tokens: 10, cost: 1 },
          { client: "codex", modelId: "gpt-5.5-mini", tokens: 3, cost: 0.25 },
        ]),
        day("2026-05-02", [{ client: "codex", modelId: "gpt-5.5", tokens: 2, cost: 0.1 }]),
      ],
      "cli"
    );

    expect(buckets).toHaveLength(1);
    expect(buckets[0]).toMatchObject({ bucketKey: "2026-05", tokens: 15 });
    expect(buckets[0].cost).toBeCloseTo(1.35, 10);

    const primaryKeys = buckets.map(
      (b) => `${b.client}|${b.origin}|${b.bucketWidth}|${b.bucketKey}`
    );
    expect(new Set(primaryKeys).size).toBe(buckets.length);
  });

  it("clamps a cost that would not survive the numeric(18,4) cast", () => {
    // toFixed(4) switches to exponential notation at 1e21, which does not
    // parse as numeric at all — the statement would error rather than store a
    // wrong number. Clamping keeps a dishonest payload from costing a real
    // device its measurement.
    const buckets = foldContributionsIntoBuckets(
      [day("2026-05-01", [{ client: "codex", tokens: 1, cost: 1e30 }])],
      "cli"
    );
    expect(buckets[0].cost).toBe(99999999999999);
    expect(buckets[0].cost.toFixed(4)).toBe("99999999999999.0000");
  });

  it("drops days whose date could not be bucketed instead of guessing", () => {
    const buckets = foldContributionsIntoBuckets(
      [
        day("2026-05-01", [{ client: "codex", tokens: 10 }]),
        day("garbage", [{ client: "codex", tokens: 999 }]),
      ],
      "cli"
    );
    expect(buckets).toEqual([
      expect.objectContaining({ bucketKey: "2026-05", tokens: 10 }),
    ]);
  });
});

describe("origin separates a backfill from a CLI submit", () => {
  // getSubmitDevice() falls back to LEGACY_SUBMIT_DEVICE_KEY when a payload
  // omits `device`, so a `tokscale import` backfill and a legacy CLI submit
  // land on the SAME submitted_devices row. Keyed without origin, GREATEST
  // would take the MAX of imported and locally-scanned history instead of
  // their sum, silently dropping whichever is smaller.
  it("tags folded rows with the submission origin", () => {
    const contributions = [day("2026-05-01", [{ client: "codex", tokens: 10 }])];
    expect(foldContributionsIntoBuckets(contributions, "cli")[0].origin).toBe("cli");
    expect(foldContributionsIntoBuckets(contributions, "backfill")[0].origin).toBe(
      "backfill"
    );
  });

  it("puts origin in the conflict key, so the two cannot collapse into one row", async () => {
    const executor = capturingExecutor();
    await recordDeviceClientTotals({
      executor,
      submittedDeviceId: "11111111-1111-4111-8111-111111111111",
      buckets: foldContributionsIntoBuckets(
        [day("2026-05-01", [{ client: "codex", tokens: 10 }])],
        "backfill"
      ),
    });

    const [query] = executor.queries;
    const conflictTarget = query.sql.match(/ON CONFLICT \(([^)]*)\)/)?.[1];
    expect(conflictTarget?.split(",").map((c) => c.trim())).toEqual([
      "submitted_device_id",
      "client",
      "origin",
      "bucket_width",
      "bucket_key",
    ]);
    expect(query.params).toContain("backfill");
  });

  it("matches the primary key the migration actually creates", () => {
    // A conflict target that does not match a real unique constraint is a
    // runtime error, so the DDL and the upsert are pinned to each other.
    const migration = readFileSync(
      resolve(__dirname, "../../src/lib/db/migrations/0022_uneven_wong.sql"),
      "utf8"
    );
    const pk = migration.match(/PRIMARY KEY\(([^)]*)\)/)?.[1];
    expect(pk?.split(",").map((c) => c.trim().replace(/"/g, ""))).toEqual([
      "submitted_device_id",
      "client",
      "origin",
      "bucket_width",
      "bucket_key",
    ]);
  });
});

describe("recordDeviceClientTotals upsert", () => {
  it("is monotonic per bucket: a partial resubmit can never lower a stored value", async () => {
    const executor = capturingExecutor();
    await recordDeviceClientTotals({
      executor,
      submittedDeviceId: "11111111-1111-4111-8111-111111111111",
      buckets: foldContributionsIntoBuckets(
        [day("2026-05-01", [{ client: "codex", tokens: 10, cost: 1 }])],
        "cli"
      ),
    });

    const [query] = executor.queries;
    expect(query.sql).toContain(
      "tokens_highwater = GREATEST(submitted_device_client_totals.tokens_highwater, EXCLUDED.tokens_highwater)"
    );
    expect(query.sql).toContain(
      "cost_highwater = GREATEST(submitted_device_client_totals.cost_highwater, EXCLUDED.cost_highwater)"
    );
    // The conflict arm must not contain a bare assignment from EXCLUDED for
    // either high-water column — that is precisely the regression.
    expect(query.sql).not.toMatch(
      /tokens_highwater\s*=\s*EXCLUDED\.tokens_highwater/
    );
    expect(query.sql).not.toMatch(/cost_highwater\s*=\s*EXCLUDED\.cost_highwater/);
  });

  it("binds the folded totals for the addressed device", async () => {
    const executor = capturingExecutor();
    await recordDeviceClientTotals({
      executor,
      submittedDeviceId: "22222222-2222-4222-8222-222222222222",
      buckets: foldContributionsIntoBuckets(
        [day("2026-05-01", [{ client: "codex", tokens: 12, cost: 0.5 }])],
        "cli"
      ),
      now: new Date("2026-05-11T00:00:00.000Z"),
    });

    expect(executor.queries).toHaveLength(1);
    expect(executor.queries[0].params).toEqual([
      "22222222-2222-4222-8222-222222222222",
      "codex",
      "cli",
      "month",
      "2026-05",
      12,
      "0.5000",
      "2026-05-11T00:00:00.000Z",
    ]);
  });

  it("issues no statement at all when the payload folds to nothing", async () => {
    const executor = capturingExecutor();
    const written = await recordDeviceClientTotals({
      executor,
      submittedDeviceId: "33333333-3333-4333-8333-333333333333",
      buckets: [],
    });
    expect(written).toBe(0);
    expect(executor.queries).toHaveLength(0);
  });
});

describe("a missing bucket reads as UNKNOWN, never as zero", () => {
  // The write runs after the submit transaction commits, so the table can lag
  // the daily rows by one submit; and it can only ever be filled by incoming
  // payloads, because backfilling it from daily_breakdown would seed it with
  // the inflated value that GREATEST then keeps forever. Either way an absent
  // bucket carries no information, and reading it as 0 fabricates a collapse.
  it("reports unknown when the user has no rows yet", () => {
    expect(
      interpretHighwaterAggregate({ bucketCount: 0, tokens: 0, cost: "0" })
    ).toEqual({ status: "unknown" });
  });

  it("reports unknown when the aggregate row is absent entirely", () => {
    expect(interpretHighwaterAggregate(undefined)).toEqual({ status: "unknown" });
    expect(interpretHighwaterAggregate(null)).toEqual({ status: "unknown" });
  });

  it("reports a known total once at least one bucket exists", () => {
    expect(
      interpretHighwaterAggregate({ bucketCount: 3, tokens: 900, cost: "1.5000" })
    ).toEqual({ status: "known", tokens: 900, cost: 1.5, bucketCount: 3 });
  });

  it("distinguishes a genuine zero total from no coverage", () => {
    expect(
      interpretHighwaterAggregate({ bucketCount: 2, tokens: 0, cost: "0" })
    ).toEqual({ status: "known", tokens: 0, cost: 0, bucketCount: 2 });
  });

  it("propagates unknown through readHighwaterTotal without inventing a zero", async () => {
    const empty = {
      execute: async () => [{ bucketCount: 0, tokens: 0, cost: "0" }],
    };
    await expect(readHighwaterTotal({ executor: empty, userId: "u" })).resolves.toEqual({
      status: "unknown",
    });
  });

  it("clamps the SUM rather than letting the bigint cast abort the statement", async () => {
    const executor = capturingExecutor();
    await readHighwaterTotal({ executor, userId: "11111111-1111-4111-8111-111111111111" });
    expect(executor.queries[0].sql).toContain(
      "LEAST(COALESCE(SUM(t.tokens_highwater), 0), 9223372036854775807)::bigint"
    );
    expect(executor.queries[0].params).toContain("month");
  });
});

describe("buildDualDerivationRecord (Phase 1.5)", () => {
  it("records both derivations and their delta once coverage exists", () => {
    const record = buildDualDerivationRecord({
      userId: "u1",
      submissionId: "s1",
      servedTokens: 1200,
      servedCost: 3,
      highwater: { status: "known", tokens: 1000, cost: 2.5, bucketCount: 4 },
    });

    expect(record).toMatchObject({
      servedTokens: 1200,
      highwaterTokens: 1000,
      tokenDelta: 200,
      tokenRatio: 1.2,
      bucketCount: 4,
      highwaterStatus: "known",
    });
  });

  it("emits a null delta, not the served total, while coverage is unknown", () => {
    const record = buildDualDerivationRecord({
      userId: "u1",
      submissionId: "s1",
      servedTokens: 1200,
      servedCost: 3,
      highwater: { status: "unknown" },
    });

    expect(record.highwaterStatus).toBe("unknown");
    expect(record.highwaterTokens).toBeNull();
    expect(record.tokenDelta).toBeNull();
    expect(record.tokenRatio).toBeNull();
    // The served value is untouched: Phase 1.5 changes nothing that is served.
    expect(record.servedTokens).toBe(1200);
  });

  it("does not divide by a zero high-water total", () => {
    const record = buildDualDerivationRecord({
      userId: "u1",
      submissionId: "s1",
      servedTokens: 500,
      servedCost: 1,
      highwater: { status: "known", tokens: 0, cost: 0, bucketCount: 2 },
    });
    expect(record.tokenRatio).toBeNull();
    expect(record.tokenDelta).toBe(500);
  });
});

describe("isDeviceClientTotalsWriteEnabled", () => {
  it("defaults to enabled when the flag is unset", () => {
    expect(isDeviceClientTotalsWriteEnabled({})).toBe(true);
  });

  it("is killable without a deploy or a migration revert", () => {
    for (const value of ["0", "false", "off", "no", "FALSE", " off "]) {
      expect(
        isDeviceClientTotalsWriteEnabled({ [DEVICE_CLIENT_TOTALS_WRITE_FLAG]: value })
      ).toBe(false);
    }
  });

  it("stays enabled for any other value", () => {
    for (const value of ["1", "true", "on", ""]) {
      expect(
        isDeviceClientTotalsWriteEnabled({ [DEVICE_CLIENT_TOTALS_WRITE_FLAG]: value })
      ).toBe(true);
    }
  });
});
