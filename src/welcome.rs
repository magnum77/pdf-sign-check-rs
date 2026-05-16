use std::sync::Arc;

use axum::{extract::State, response::Html};

use crate::webhook::AppState;

pub async fn page(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(render(&state.config.webhook_path))
}

fn render(webhook_path: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>pdf-sign-check-rs</title>
  <style>
    :root {{
      color-scheme: dark;
      --bg: #07111f;
      --panel: rgba(10, 19, 36, 0.88);
      --panel-border: rgba(148, 163, 184, 0.16);
      --text: #e5eefb;
      --muted: #9fb0c8;
      --accent: #38bdf8;
      --accent-2: #8b5cf6;
    }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0;
      min-height: 100vh;
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      color: var(--text);
      background:
        radial-gradient(circle at top left, rgba(56, 189, 248, 0.24), transparent 35%),
        radial-gradient(circle at bottom right, rgba(139, 92, 246, 0.18), transparent 30%),
        linear-gradient(180deg, #08111f 0%, #030712 100%);
    }}
    .wrap {{
      min-height: 100vh;
      display: grid;
      place-items: center;
      padding: 32px;
    }}
    .card {{
      width: min(720px, 100%);
      padding: 40px;
      border: 1px solid var(--panel-border);
      border-radius: 28px;
      background: var(--panel);
      box-shadow: 0 20px 70px rgba(0, 0, 0, 0.35);
      backdrop-filter: blur(18px);
    }}
    .eyebrow {{
      display: inline-flex;
      align-items: center;
      gap: 10px;
      text-transform: uppercase;
      letter-spacing: 0.15em;
      font-size: 12px;
      color: var(--muted);
    }}
    .eyebrow::before {{
      content: "";
      width: 10px;
      height: 10px;
      border-radius: 999px;
      background: var(--accent);
      box-shadow: 0 0 18px rgba(56, 189, 248, 0.75);
    }}
    h1 {{
      margin: 18px 0 14px;
      font-size: clamp(40px, 6vw, 64px);
      line-height: 0.98;
      letter-spacing: -0.04em;
    }}
    p {{
      margin: 0;
      color: var(--muted);
      line-height: 1.65;
      font-size: 16px;
    }}
    .actions {{
      display: flex;
      flex-wrap: wrap;
      gap: 14px;
      margin-top: 28px;
    }}
    a {{
      display: inline-flex;
      align-items: center;
      justify-content: center;
      min-height: 48px;
      padding: 0 18px;
      border-radius: 14px;
      text-decoration: none;
      font-weight: 600;
      border: 1px solid transparent;
      transition: transform 120ms ease, border-color 120ms ease, background 120ms ease;
    }}
    a:hover {{
      transform: translateY(-1px);
    }}
    .primary {{
      color: #03111f;
      background: linear-gradient(135deg, var(--accent), #7dd3fc);
    }}
    .secondary {{
      color: var(--text);
      border-color: var(--panel-border);
      background: rgba(15, 23, 42, 0.5);
    }}
    .meta {{
      display: grid;
      gap: 10px;
      margin-top: 30px;
      color: var(--muted);
      font-size: 14px;
    }}
    code {{
      padding: 2px 8px;
      border-radius: 999px;
      background: rgba(148, 163, 184, 0.14);
      color: var(--text);
    }}
  </style>
</head>
<body>
  <main class="wrap">
    <section class="card">
      <div class="eyebrow">pdf-sign-check-rs</div>
      <h1>Welcome.</h1>
      <p>
        This service watches Paperless for signed PDFs, updates matching documents,
        and exposes a browser dashboard for manual scans.
      </p>
      <div class="actions">
        <a class="primary" href="/scan">Open scan dashboard</a>
        <a class="secondary" href="{webhook_path}">Webhook endpoint</a>
      </div>
      <div class="meta">
        <div>Health check: <code>/healthz</code></div>
        <div>Scan dashboard: <code>/scan</code></div>
      </div>
    </section>
  </main>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn welcome_page_links_to_scan() {
        let html = render("/webhook");
        assert!(html.contains(r#"href="/scan""#));
        assert!(html.contains(r#"href="/webhook""#));
    }
}
