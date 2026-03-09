    let to_str = "2N4tPA5sdfw5uR4x5mM8G846AyvfdfJZGsCAXj25D7VP";
    let to_pubkey = Pubkey::from_str(to_str)?;
    let idx = (random::<u64>() as usize) % JITO_TIP_ACCOUNTS.len();
    let tip_account = Pubkey::from_str(&JITO_TIP_ACCOUNTS[idx])?;
    let transfer_lamports = 5_000;
    let tip_lamports = 5_000;
    let cu_limit: u32 = 200_000;
    let cu_price = 1;

    let blockhash = rpc_client
        .get_latest_blockhash()
        .await
        .context("获取 Jito smoke test blockhash 失败")?;

    let instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
        ComputeBudgetInstruction::set_compute_unit_price(cu_price),
        transfer(&payer.pubkey(), &to_pubkey, transfer_lamports),
        transfer(&payer.pubkey(), &tip_account, tip_lamports),
    ];

    let v0_message = V0Message::try_compile(&payer.pubkey(), &instructions, &[], blockhash)
        .context("构建 Jito smoke test 消息失败")?;
    let message = VersionedMessage::V0(v0_message);
    let tx =
        VersionedTransaction::try_new(message, &[&*payer]).context("签名 Jito smoke test 失败")?;

    let tx_bytes = serialize_versioned_tx(&tx)?;
    let tx_b64 = general_purpose::STANDARD.encode(&tx_bytes);
    let frankfurt_url = "https://frankfurt.mainnet.block-engine.jito.wtf:443/api/v1/bundles";
    info!("url:{frankfurt_url}");
    let bundle_json = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [[tx_b64], { "encoding": "base64" }]
    });

    let start = Instant::now();
    let mut retry_count: u64 = 0;
    let body_text = loop {
        let resp = client_direct
            .post(frankfurt_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&bundle_json)
            .send()
            .await
            .context("请求 Frankfurt bundle 接口失败")?;

        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            break body_text;
        }

        if status.as_u16() == 429 {
            retry_count += 1;
            warn!(
                "Frankfurt bundle 命中 429，1 秒后重试（第 {} 次）, body: {}",
                retry_count,
                body_text
            );
            sleep(Duration::from_secs(1)).await;
            continue;
        }

        return Err(anyhow!(
            "Frankfurt bundle 发送失败, status: {}, body: {}",
            status,
            body_text
        ));
    };
