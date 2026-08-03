/**
 * Per-device, per-client, per-bucket token/cost high-water marks.
 *
 * Phase 1 + Phase 1.5 of `docs/ratchet-inflation-recovery.md`.
 *
 * Phase 1 POPULATES `submitted_device_client_totals` and nothing reads it.
 * Phase 1.5 derives a submission total from it purely so the divergence from
 * the served `SUM(daily_breakdown.tokens)` can be measured on live traffic.
 * **No value served to any caller is derived from this module.**
 *
 * Everything here is deliberately independent of the submit transaction: the
 * write is a `GREATEST` upsert and therefore idempotent, so it runs AFTER the
 * submit commits. A failure there costs one deferred measurement that the next
 * submit repairs, instead of rejecting the user's submission.
 */

import { sql, type SQL } from "drizzle-orm";
import { clientContributionToBreakdownData } from "./helpers";

/**
 * Only one bucket width is written today. The doc allows `week` alongside
 * `month`; starting with month alone halves the write volume (measured
 * 2026-07-31: p50 9 / p95 32 / max 91 rows per full-history submit at month
 * only, against p50 33 / p95 122 / max 349 at both widths), and Phase 3 makes
 * the width question far less interesting by permitting daily buckets.
 */
export const DEVICE_CLIENT_TOTALS_BUCKET_WIDTH = "month";

/**
 * Kill switch. The migration is effectively irreversible (drizzle stores the
 * content hash of an applied migration), but the WRITE need not inherit that:
 * setting `TOKSCALE_DEVICE_CLIENT_TOTALS_WRITE` to `0`/`false`/`off` disables
 * it in seconds without reverting a migration or shipping a rollback.
 *
 * Default is ENABLED — the flag exists to switch a misbehaving write off, not
 * to gate it on.
 */
export const DEVICE_CLIENT_TOTALS_WRITE_FLAG = "TOKSCALE_DEVICE_CLIENT_TOTALS_WRITE";

const DISABLED_FLAG_VALUES = new Set(["0", "false", "off", "no"]);

/**
 * PostgreSQL caps a statement at 65,535 bound parameters. Each row binds 7,
 * so this stays an order of magnitude clear even for the max-cardinality
 * submit observed in production (91 rows at month width).
 */
const UPSERT_CHUNK_SIZE = 1000;

/** int8 max. Mirrors the clamps already applied in the submit route. */
const BIGINT_MAX = "9223372036854775807";

export type DeviceClientTotalsOrigin = "cli" | "backfill";

export interface DeviceClientBucketTotal {
  client: string;
  origin: DeviceClientTotalsOrigin;
  bucketWidth: string;
  bucketKey: string;
  tokens: number;
  cost: number;
}

/**
 * Structural shape of one day of an already-validated payload. Kept structural
 * rather than importing the Zod-inferred type so this module can be exercised
 * without constructing a full `SubmissionData`.
 */
export interface DeviceClientTotalsContribution {
  date: string;
  clients: Array<{
    client: string;
    modelId: string;
    messages: number;
    cost: number;
    tokens: {
      input: number;
      output: number;
      cacheRead: number;
      cacheWrite: number;
      reasoning?: number;
    };
  }>;
}

export function isDeviceClientTotalsWriteEnabled(
  env: Record<string, string | undefined> = process.env
): boolean {
  const raw = env[DEVICE_CLIENT_TOTALS_WRITE_FLAG];
  if (raw == null) return true;
  return !DISABLED_FLAG_VALUES.has(raw.trim().toLowerCase());
}

/**
 * `YYYY-MM-DD` -> `YYYY-MM`. Returns null for anything that is not a plain
 * ISO calendar date, so a malformed date can never invent a bucket. The
 * validation schema already pins the format; this is the module's own guard so
 * it holds for callers that bypass it.
 *
 * No timezone conversion happens here, deliberately. The stored `date` is
 * whatever calendar day the CLI attributed the usage to, and re-deriving it
 * under a different zone is the exact mechanism this table exists to measure
 * (see Phase 3, which pins the bucket key).
 */
export function monthBucketKey(date: string): string | null {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(date)) return null;
  const month = Number(date.slice(5, 7));
  if (month < 1 || month > 12) return null;
  return date.slice(0, 7);
}

function clampTokens(value: number): number {
  if (!Number.isFinite(value) || value <= 0) return 0;
  // Values arrive as JS numbers, so precision (not int8 range) is the binding
  // limit. Clamping at MAX_SAFE_INTEGER keeps the bound parameter exact and
  // stays far below int8 max.
  return Math.min(Math.round(value), Number.MAX_SAFE_INTEGER);
}

/**
 * numeric(18,4) holds up to 99999999999999.9999. A month bucket sums day costs
 * that each already fit `daily_breakdown.cost`'s numeric(14,4) (max ~1e10), so
 * an honest bucket is ~4 orders of magnitude clear of this. The clamp exists
 * for the dishonest case: `toFixed(4)` switches to exponential notation at
 * 1e21, which would not parse as numeric at all.
 */
const COST_HIGHWATER_MAX = 99999999999999;

function clampCost(value: number): number {
  if (!Number.isFinite(value) || value <= 0) return 0;
  return Math.min(value, COST_HIGHWATER_MAX);
}

/**
 * Fold a payload's daily contributions into (client, origin, bucket) totals.
 *
 * Token arithmetic goes through `clientContributionToBreakdownData` — the same
 * helper the daily rows use — so the two derivations cannot drift apart on
 * what counts as a token.
 *
 * `origin` is applied uniformly from the submission-level provenance tag,
 * matching how the submit route stamps `origin` into every client entry of a
 * backfill submission.
 */
export function foldContributionsIntoBuckets(
  contributions: readonly DeviceClientTotalsContribution[],
  origin: DeviceClientTotalsOrigin,
  bucketWidth: string = DEVICE_CLIENT_TOTALS_BUCKET_WIDTH
): DeviceClientBucketTotal[] {
  const byBucket = new Map<string, DeviceClientBucketTotal>();

  for (const day of contributions) {
    const bucketKey = monthBucketKey(day.date);
    if (bucketKey == null) continue;

    for (const contribution of day.clients) {
      const breakdown = clientContributionToBreakdownData(contribution);
      // U+0000 cannot occur in a validated client id or a bucket key, so the
      // composite map key is unambiguous. Written as an escape, not a
      // literal control character, so the source stays plain text.
      const mapKey = `${contribution.client}\u0000${bucketKey}`;
      const existing = byBucket.get(mapKey);

      if (existing) {
        existing.tokens += breakdown.tokens || 0;
        existing.cost += breakdown.cost || 0;
        continue;
      }

      byBucket.set(mapKey, {
        client: contribution.client,
        origin,
        bucketWidth,
        bucketKey,
        tokens: breakdown.tokens || 0,
        cost: breakdown.cost || 0,
      });
    }
  }

  return Array.from(byBucket.values(), (bucket) => ({
    ...bucket,
    tokens: clampTokens(bucket.tokens),
    cost: clampCost(bucket.cost),
  }));
}

export interface DeviceClientTotalsExecutor {
  execute(query: SQL): Promise<unknown>;
}

/**
 * `GREATEST` upsert of the folded buckets for one device.
 *
 * Monotonic per bucket: a `--clients`/`--date` filtered submit reports only a
 * slice of a month and must never lower the stored value. That is also what
 * makes the write idempotent, and therefore safe to run after commit.
 *
 * Returns the number of rows sent (not the number actually raised).
 */
export async function recordDeviceClientTotals(params: {
  executor: DeviceClientTotalsExecutor;
  submittedDeviceId: string;
  buckets: readonly DeviceClientBucketTotal[];
  now?: Date;
}): Promise<number> {
  const { executor, submittedDeviceId, buckets } = params;
  if (buckets.length === 0) return 0;

  const updatedAt = (params.now ?? new Date()).toISOString();

  for (let i = 0; i < buckets.length; i += UPSERT_CHUNK_SIZE) {
    const chunk = buckets.slice(i, i + UPSERT_CHUNK_SIZE);
    const valuesClauses = chunk.map(
      (bucket) =>
        sql`(${submittedDeviceId}::uuid, ${bucket.client}, ${bucket.origin}, ${bucket.bucketWidth}, ${bucket.bucketKey}, ${bucket.tokens}::bigint, ${bucket.cost.toFixed(4)}::numeric(18,4), ${updatedAt}::timestamptz)`
    );

    await executor.execute(sql`
      INSERT INTO submitted_device_client_totals (
        submitted_device_id, client, origin, bucket_width, bucket_key,
        tokens_highwater, cost_highwater, updated_at
      )
      VALUES ${sql.join(valuesClauses, sql`, `)}
      ON CONFLICT (submitted_device_id, client, origin, bucket_width, bucket_key) DO UPDATE SET
        tokens_highwater = GREATEST(submitted_device_client_totals.tokens_highwater, EXCLUDED.tokens_highwater),
        cost_highwater = GREATEST(submitted_device_client_totals.cost_highwater, EXCLUDED.cost_highwater),
        updated_at = EXCLUDED.updated_at
    `);
  }

  return buckets.length;
}

/**
 * A high-water reconstruction of a user's total.
 *
 * `unknown` is NOT zero. The write runs after the submit transaction commits,
 * so the table can lag the daily rows by one submit; and it can only ever be
 * filled by incoming payloads (backfilling it from `daily_breakdown` would
 * seed it with the inflated value that `GREATEST` then keeps forever). Both
 * mean an absent bucket carries no information about usage, and any consumer
 * that reads it as 0 would report a fabricated collapse.
 *
 * `known` means "at least one bucket exists" — it does NOT mean coverage is
 * complete. During the forced warm-up a user can easily have one bucket out of
 * fifty, which reads as `known` with a total far below the served one. That is
 * the correct behaviour for a MEASUREMENT (the gap is the signal) but it makes
 * `status` unsafe as a Phase 2 readiness gate on its own. `bucketCount` is
 * carried for exactly that reason: the gate belongs on coverage and on the
 * observed delta, never on `status === "known"`.
 */
export type HighwaterTotalReading =
  | { status: "unknown" }
  | { status: "known"; tokens: number; cost: number; bucketCount: number };

export interface HighwaterAggregateRow {
  bucketCount: number;
  tokens: number;
  cost: string;
}

/**
 * Both derivations, read in ONE statement so they share a snapshot.
 *
 * `snapshotTokens`/`snapshotCost` are `SUM(daily_breakdown)` re-read at census
 * time — NOT the value served to the caller. See `readDualDerivation` for why
 * the difference matters.
 */
export interface DualDerivationRow extends HighwaterAggregateRow {
  snapshotTokens: number;
  snapshotCost: string;
}

export function interpretHighwaterAggregate(
  row: HighwaterAggregateRow | undefined | null
): HighwaterTotalReading {
  if (!row) return { status: "unknown" };
  const bucketCount = Number(row.bucketCount ?? 0);
  if (!Number.isFinite(bucketCount) || bucketCount <= 0) {
    return { status: "unknown" };
  }
  const cost = Number.parseFloat(row.cost ?? "0");
  return {
    status: "known",
    tokens: Number(row.tokens ?? 0),
    cost: Number.isFinite(cost) ? cost : 0,
    bucketCount,
  };
}

export interface HighwaterQueryExecutor {
  execute(query: SQL): Promise<unknown>;
}

function firstRow(result: unknown): HighwaterAggregateRow | undefined {
  if (Array.isArray(result)) return result[0] as HighwaterAggregateRow | undefined;
  if (result && typeof result === "object" && Array.isArray((result as { rows?: unknown }).rows)) {
    return (result as { rows: unknown[] }).rows[0] as HighwaterAggregateRow | undefined;
  }
  return undefined;
}

/**
 * Phase 1.5 read side: sum the high-water marks across every device a user
 * owns, at the single bucket width Phase 1 writes.
 *
 * Summing ACROSS origins is intentional — `cli` and `backfill` history are
 * additive, which is exactly why `origin` is in the primary key instead of
 * being collapsed by `GREATEST`.
 *
 * `SUM(int8)` widens to numeric, so the cast back to bigint would raise "out
 * of range" on a pathological total and abort the statement. `LEAST` clamps
 * instead — the same treatment the submit route applies to its own aggregates.
 * `SUM(numeric)` needs no clamp; numeric is arbitrary precision.
 */
export async function readHighwaterTotal(params: {
  executor: HighwaterQueryExecutor;
  userId: string;
  bucketWidth?: string;
}): Promise<HighwaterTotalReading> {
  const bucketWidth = params.bucketWidth ?? DEVICE_CLIENT_TOTALS_BUCKET_WIDTH;
  const result = await params.executor.execute(sql`
    SELECT
      COUNT(*)::int AS "bucketCount",
      LEAST(COALESCE(SUM(t.tokens_highwater), 0), ${sql.raw(BIGINT_MAX)})::bigint AS "tokens",
      COALESCE(SUM(t.cost_highwater), 0)::text AS "cost"
    FROM submitted_device_client_totals AS t
    JOIN submitted_devices AS d ON d.id = t.submitted_device_id
    WHERE d.user_id = ${params.userId}::uuid
      AND t.bucket_width = ${bucketWidth}
  `);

  return interpretHighwaterAggregate(firstRow(result));
}

/**
 * Read BOTH derivations in a single statement.
 *
 * Why this is not just `readHighwaterTotal` plus the value from the response:
 * the submit transaction takes `.for('update')` on the submissions row, so two
 * devices belonging to one user serialize only UNTIL COMMIT. The census runs
 * after that, unsynchronized. So request A can hold a served total captured at
 * its own commit while request B commits and raises the high-water rows, and
 * A's later high-water read then sees B's contribution. Pairing those two
 * numbers reports a divergence that neither derivation actually had — and the
 * skew goes the wrong way, understating the served side, so it can log a
 * NEGATIVE delta while both derivations agreed at every commit.
 *
 * That noise lands specifically on users submitting from multiple devices,
 * which is exactly the population the census exists to characterize, so it is
 * not tolerable in the data meant to gate the Phase 2 cutover.
 *
 * A single statement evaluates every subquery in one snapshot, so the two
 * sides are consistent by construction. The value SERVED to the caller is
 * still the one computed inside the transaction and is not touched here; it is
 * recorded alongside so a concurrent submit is visible rather than silent.
 */
export async function readDualDerivation(params: {
  executor: HighwaterQueryExecutor;
  userId: string;
  submissionId: string;
  bucketWidth?: string;
}): Promise<{ snapshotTokens: number; snapshotCost: number; highwater: HighwaterTotalReading }> {
  const bucketWidth = params.bucketWidth ?? DEVICE_CLIENT_TOTALS_BUCKET_WIDTH;
  const result = await params.executor.execute(sql`
    SELECT
      (
        SELECT LEAST(COALESCE(SUM(db.tokens), 0), ${sql.raw(BIGINT_MAX)})::bigint
        FROM daily_breakdown AS db
        WHERE db.submission_id = ${params.submissionId}::uuid
      ) AS "snapshotTokens",
      (
        SELECT COALESCE(SUM(CAST(db.cost AS DECIMAL(14,4))), 0)::text
        FROM daily_breakdown AS db
        WHERE db.submission_id = ${params.submissionId}::uuid
      ) AS "snapshotCost",
      COUNT(*)::int AS "bucketCount",
      LEAST(COALESCE(SUM(t.tokens_highwater), 0), ${sql.raw(BIGINT_MAX)})::bigint AS "tokens",
      COALESCE(SUM(t.cost_highwater), 0)::text AS "cost"
    FROM submitted_device_client_totals AS t
    JOIN submitted_devices AS d ON d.id = t.submitted_device_id
    WHERE d.user_id = ${params.userId}::uuid
      AND t.bucket_width = ${bucketWidth}
  `);

  const row = firstRow(result) as DualDerivationRow | undefined;
  const snapshotCost = Number.parseFloat(row?.snapshotCost ?? "0");
  return {
    snapshotTokens: Number(row?.snapshotTokens ?? 0),
    snapshotCost: Number.isFinite(snapshotCost) ? snapshotCost : 0,
    highwater: interpretHighwaterAggregate(row),
  };
}

/**
 * The Phase 1.5 record: the pair of derivations, and the delta between them.
 *
 * The SERVED value is `servedTokens`/`servedCost` — `SUM(daily_breakdown)`,
 * exactly as before. The high-water side is recorded and never returned.
 */
export interface DualDerivationRecord {
  userId: string;
  submissionId: string;
  bucketWidth: string;
  /**
   * What the HTTP response actually carried, computed inside the transaction.
   * Recorded as evidence that the served value is unchanged — never used to
   * compute the delta, because it comes from a different snapshot than the
   * high-water read (see `readDualDerivation`).
   */
  servedTokens: number;
  servedCost: number;
  /** `SUM(daily_breakdown)` from the SAME snapshot as the high-water read. */
  snapshotTokens: number;
  snapshotCost: number;
  /**
   * True when a concurrent submit for this user landed between this request's
   * commit and its census read. Not an error — it is why the delta is computed
   * from `snapshotTokens` rather than `servedTokens`.
   */
  racedConcurrentSubmit: boolean;
  highwaterStatus: HighwaterTotalReading["status"];
  highwaterTokens: number | null;
  highwaterCost: number | null;
  bucketCount: number | null;
  /** snapshotTokens - highwaterTokens; null while the reading is unknown. */
  tokenDelta: number | null;
  /** snapshotTokens / highwaterTokens; null while unknown or divide-by-zero. */
  tokenRatio: number | null;
}

export function buildDualDerivationRecord(params: {
  userId: string;
  submissionId: string;
  bucketWidth?: string;
  servedTokens: number;
  servedCost: number;
  snapshotTokens: number;
  snapshotCost: number;
  highwater: HighwaterTotalReading;
}): DualDerivationRecord {
  const { highwater } = params;
  const base = {
    userId: params.userId,
    submissionId: params.submissionId,
    bucketWidth: params.bucketWidth ?? DEVICE_CLIENT_TOTALS_BUCKET_WIDTH,
    servedTokens: params.servedTokens,
    servedCost: params.servedCost,
    snapshotTokens: params.snapshotTokens,
    snapshotCost: params.snapshotCost,
    racedConcurrentSubmit: params.snapshotTokens !== params.servedTokens,
  };

  if (highwater.status === "unknown") {
    return {
      ...base,
      highwaterStatus: "unknown",
      highwaterTokens: null,
      highwaterCost: null,
      bucketCount: null,
      tokenDelta: null,
      tokenRatio: null,
    };
  }

  return {
    ...base,
    highwaterStatus: "known",
    highwaterTokens: highwater.tokens,
    highwaterCost: highwater.cost,
    bucketCount: highwater.bucketCount,
    // Both operands come from one snapshot, so this cannot report a divergence
    // that neither derivation had.
    tokenDelta: params.snapshotTokens - highwater.tokens,
    tokenRatio:
      highwater.tokens > 0 ? params.snapshotTokens / highwater.tokens : null,
  };
}

/**
 * Stable prefix so the pair is greppable in platform logs. The same comparison
 * is also reconstructable offline at any time from
 * `submissions.total_tokens` against `SUM(tokens_highwater)`, since both sides
 * are persisted — the log exists to surface divergence on live traffic without
 * anyone running a script, not because the data would otherwise be lost.
 */
export const DUAL_DERIVATION_LOG_PREFIX = "ratchet-census";
