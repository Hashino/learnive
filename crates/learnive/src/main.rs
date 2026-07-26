//! learnive — servidor local do documento vivo.
//!
//! Topologia (§3): backend Rust como servidor HTTP local, renderizado no
//! navegador real do usuário. Toda I/O de arquivo é do backend; o navegador só
//! fala com o backend por localhost. Segurança do servidor local em `security`
//! (§3.1): bind só em 127.0.0.1, token de sessão obrigatório, Origin/Host
//! restritos, nenhuma mutação em GET.

mod app;
mod security;

use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    // Porta fixa amigável por padrão; sobrescrevível por env para dev/testes.
    let port: u16 = std::env::var("LEARNIVE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7420);

    let state = app::AppState::new(port);
    let router = app::build_router(state.clone());

    // Bind exclusivamente em 127.0.0.1, nunca 0.0.0.0 (§3.1).
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("não foi possível ligar em http://127.0.0.1:{port}: {e}");
            eprintln!("defina LEARNIVE_PORT para escolher outra porta.");
            std::process::exit(1);
        }
    };

    println!("learnive rodando.");
    println!(
        "Abra no navegador: http://127.0.0.1:{port}/?token={}",
        state.token
    );
    println!("O token de sessão é obrigatório em toda requisição (§3.1).");

    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("servidor encerrou com erro: {e}");
        std::process::exit(1);
    }
}
