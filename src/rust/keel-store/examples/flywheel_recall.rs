//! Does the REAL symbol extractor hold the flywheel's retrieval recall@k? (Linear NEW-1076.)
//!
//! The validated retrieval (0→72%, recall@3=100%) used HAND-MODELED session↔symbol/pattern tags.
//! This is the falsification the ticket demands: recreate the exact 12-item pool (6 targets + 6
//! lexical-trap distractors) with REAL code, commit each session as a keel change, extract its `sym`
//! set with the production `changed_symbols`, extract each task's `sym` from its target stub with
//! `symbols_from_text`, run the identical deterministic graph scoring (2·|Δsym| + 2·|Δpat|), and
//! measure recall@1/2/3 — the bar is the right session in the top-k over the traps.
//!
//! Two modes isolate what the pattern half must add: `sym-only` (pat contributes 0, since the `pat`
//! classifier is increment 2) and `sym+pat` (pat still hand-modeled, as in the validated lab). No
//! LLM / API key — recall@k is a pure function of the tags. Run: `cargo run -p keel-store --release
//! --example flywheel_recall`.

use keel_store::sessiontag::{changed_symbols, symbols_from_text};
use keel_store::Repo;
use std::collections::BTreeSet;

/// One pool member. `code` is the actual change the session made (committed, then symbol-extracted).
/// Targets also carry a `query` (the new task's target stub + description) and its hand-modeled `pat`.
struct Item {
    id: &'static str,
    code: &'static str,
    pat: &'static [&'static str],
    query: Option<Query>,
}
struct Query {
    stub: &'static str,
    task: &'static str,
    pat: &'static [&'static str],
}

fn pool() -> Vec<Item> {
    vec![
        // ── 6 targets: a prior session (code) + a new task (query) that should retrieve it ──
        Item {
            id: "T-gateway",
            code: "export async function chargePayment(gateway, order) {\n  \
                   await sleep(50); // gateway returns a stale balance without a settle delay\n  \
                   return gateway.charge(order.amount);\n}",
            pat: &["external-call"],
            query: Some(Query {
                stub: "export async function refund(gateway, order) {\n  // TODO: refund via gateway.refund()\n}",
                task: "Implement refund to refund an order via the payment gateway.",
                pat: &["external-call"],
            }),
        },
        Item {
            id: "T-uuid",
            code: "export function migrateUsers(db) {\n  \
                   // policy: all new tables use uuidv7, never auto-increment\n  \
                   db.alterColumn('users', 'id', uuidv7());\n}",
            pat: &["schema-create"],
            query: Some(Query {
                stub: "export function createOrdersTable(db) {\n  // TODO: define the orders table schema\n}",
                task: "Define the schema for a new orders table.",
                pat: &["schema-create"],
            }),
        },
        Item {
            id: "T-analytics",
            code: "export async function recordPurchase(order) {\n  \
                   // never call analytics directly; enqueue to avoid cascading outages\n  \
                   await analyticsQueue.enqueue({ event: 'purchase', order });\n}",
            pat: &["external-call"],
            query: Some(Query {
                stub: "export async function onSignup(user) {\n  await saveUser(user);\n  // TODO: track the signup event\n}",
                task: "Add event tracking for the signup.",
                pat: &["external-call"],
            }),
        },
        Item {
            id: "T-money",
            code: "export function priceTotal(items) {\n  \
                   // all money is integer cents, never floats — avoids rounding errors\n  \
                   return items.reduce((cents, it) => cents + it.priceCents, 0);\n}",
            pat: &["money"],
            query: Some(Query {
                stub: "export function addLineItem(invoice, name, price) {\n  // TODO: add a priced line item\n}",
                task: "Implement addLineItem to add a priced line item to an invoice.",
                pat: &["money"],
            }),
        },
        Item {
            id: "T-tenant",
            code: "export async function fetchAccounts(db, tenantId) {\n  \
                   // every query MUST filter by tenantId (cross-tenant leak otherwise)\n  \
                   return db.query('accounts').where({ tenantId });\n}",
            pat: &["db-query"],
            query: Some(Query {
                stub: "export async function listReports(db, opts) {\n  // TODO: query and return reports\n}",
                task: "Implement listReports to fetch reports from the db.",
                pat: &["db-query"],
            }),
        },
        Item {
            id: "T-idem",
            code: "export async function sendEmail(provider, to, body) {\n  \
                   // any retried external call must pass an idempotency key\n  \
                   return retry(() => provider.send({ to, body, idempotencyKey: uuid() }));\n}",
            pat: &["external-call"],
            query: Some(Query {
                stub: "export async function sendSms(provider, to, body) {\n  // TODO: send with retries on failure\n}",
                task: "Implement sendSms with retries on failure.",
                pat: &["external-call"],
            }),
        },
        // ── 6 lexical-trap distractors: share domain symbols with a target but the WRONG lesson ──
        Item {
            id: "D-gwtimeout",
            code: "export function configureGateway(gateway) {\n  \
                   // raise the payment gateway HTTP timeout under load\n  \
                   gateway.httpTimeout = 10000;\n}",
            pat: &["external-call"],
            query: None,
        },
        Item {
            id: "D-ordersidx",
            code: "export function indexOrders(db) {\n  \
                   // speed up slow orders queries with a composite index\n  \
                   db.createIndex('orders', ['customerId', 'createdAt']);\n}",
            pat: &["db-query"],
            query: None,
        },
        Item {
            id: "D-welcome",
            code: "export async function onSignup(user) {\n  await saveUser(user);\n  \
                   // send a welcome email after saving the user\n  \
                   await emailService.send(user.email, welcomeTemplate);\n}",
            pat: &["external-call"],
            query: None,
        },
        Item {
            id: "D-invoicetax",
            code: "export function addTaxLine(invoice) {\n  \
                   // invoices must include tax as a separate line item\n  \
                   invoice.lines.push({ kind: 'tax', amount: regionalTax(invoice) });\n}",
            pat: &["money"],
            query: None,
        },
        Item {
            id: "D-reportspage",
            code: "export async function pageReports(db, cursor) {\n  \
                   // use keyset (cursor) pagination for the reports list\n  \
                   return db.query('reports').keyset(cursor);\n}",
            pat: &["db-query"],
            query: None,
        },
        Item {
            id: "D-smstmpl",
            code: "export function renderSms(body, locale) {\n  \
                   // localize every sms body through the i18n template engine before sending\n  \
                   return i18n.render(body, locale);\n}",
            pat: &["external-call"],
            query: None,
        },
    ]
}

/// A pool member after extraction: its id, extracted `sym` set, and `pat` set.
struct Tagged {
    id: String,
    sym: BTreeSet<String>,
    pat: BTreeSet<String>,
}

/// score = 2·|shared sym| + 2·|shared pat|; deterministic sort, id tiebreak — the validated ranking.
fn rank(query_sym: &BTreeSet<String>, query_pat: &BTreeSet<String>, pool: &[Tagged], use_pat: bool) -> Vec<String> {
    let mut scored: Vec<(i32, &str)> = pool
        .iter()
        .map(|c| {
            let s = c.sym.intersection(query_sym).count() as i32;
            let p = if use_pat { c.pat.intersection(query_pat).count() as i32 } else { 0 };
            (2 * s + 2 * p, c.id.as_str())
        })
        .collect();
    // higher score first; ties broken by id ascending (matches the lab's localeCompare)
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    scored.into_iter().map(|(_, id)| id.to_string()).collect()
}

fn main() {
    let dir = std::env::temp_dir().join(format!("keel-flywheel-recall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let repo = Repo::open(&dir.join("store")).unwrap();

    let items = pool();

    // Commit each session's code as its own change (a distinct file per item, so each change's diff
    // is exactly that session's code), and extract its sym set from the real production extractor.
    let mut tagged: Vec<Tagged> = Vec::new();
    let mut queries: Vec<(String, BTreeSet<String>, BTreeSet<String>)> = Vec::new(); // (target id, qsym, qpat)
    for (i, it) in items.iter().enumerate() {
        std::fs::write(work.join(format!("{}.js", it.id)), it.code).unwrap();
        let c = repo.commit_dir(&work, it.id, "bench", (i + 1) as u64, None).unwrap();
        let sym = changed_symbols(&repo, c).unwrap();
        tagged.push(Tagged { id: it.id.to_string(), sym, pat: it.pat.iter().map(|s| s.to_string()).collect() });

        if let Some(q) = &it.query {
            let qsym = symbols_from_text(&format!("{}\n{}", q.stub, q.task));
            queries.push((it.id.to_string(), qsym, q.pat.iter().map(|s| s.to_string()).collect()));
        }
    }

    println!("╔═══ flywheel retrieval recall@k — REAL extractor (NEW-1076) ═══");
    println!("    pool = {} sessions ({} targets + {} distractors), scoring = 2·|Δsym| + 2·|Δpat|, top-k",
        tagged.len(), queries.len(), tagged.len() - queries.len());
    println!("    baseline to beat: hand-modeled tags gave recall@1=0, recall@2=83%, recall@3=100%\n");

    for (mode, use_pat) in [("sym-only", false), ("sym+pat", true)] {
        let (mut r1, mut r2, mut r3) = (0, 0, 0);
        println!("    ── {mode} ──");
        for (tid, qsym, qpat) in &queries {
            let ranked = rank(qsym, qpat, &tagged, use_pat);
            let pos = ranked.iter().position(|id| id == tid).map(|p| p + 1).unwrap_or(usize::MAX);
            if pos <= 1 { r1 += 1; }
            if pos <= 2 { r2 += 1; }
            if pos <= 3 { r3 += 1; }
            let top: Vec<&str> = ranked.iter().take(3).map(|s| s.as_str()).collect();
            println!("    {:<11} right@{:<2} top3: {}", tid, if pos == usize::MAX { 0 } else { pos }, top.join(", "));
        }
        let n = queries.len();
        let pct = |x: usize| format!("{:.0}%", 100.0 * x as f64 / n as f64);
        println!("    recall@1={}  recall@2={}  recall@3={}\n", pct(r1), pct(r2), pct(r3));
    }

    println!("    Read it: recall@3 = 100% means the right prior session is always in the top-3 the");
    println!("    agent sees (a strong answerer disambiguates the trap). sym-only vs sym+pat shows how");
    println!("    much the pattern classifier (increment 2) must add — the pattern-retrieved cases");
    println!("    (schema-create/db-query with disjoint symbols) are exactly where sym-only should drop.");
    println!("╚═══ done ═══");
    let _ = std::fs::remove_dir_all(&dir);
}
