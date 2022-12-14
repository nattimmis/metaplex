use std::{
    fs::{self, File},
    io::{Read, Write},
};

use serde_json::Value;
use solana_client::{
    client_error::reqwest,
    rpc_config::{
        RpcAccountInfoConfig, RpcLargestAccountsConfig, RpcLargestAccountsFilter,
        RpcProgramAccountsConfig,
    },
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_program::system_instruction;
use solana_sdk::{
    account::ReadableAccount,
    commitment_config::{CommitmentConfig, CommitmentLevel},
};
use spl_token_metadata::state::MAX_METADATA_LEN;
use std::convert::TryFrom;
use {
    arrayref::array_ref,
    clap::{crate_description, crate_name, crate_version, App, Arg, ArgMatches, SubCommand},
    solana_clap_utils::{
        input_parsers::pubkey_of,
        input_validators::{is_url, is_valid_pubkey, is_valid_signer},
    },
    solana_client::rpc_client::RpcClient,
    solana_client::rpc_request::TokenAccountsFilter,
    solana_program::{
        account_info::AccountInfo, borsh::try_from_slice_unchecked, program_pack::Pack,
    },
    solana_sdk::{
        pubkey::Pubkey,
        signature::{read_keypair_file, Keypair, Signer},
        system_instruction::create_account,
        transaction::Transaction,
    },
    spl_token::{
        instruction::{initialize_account, initialize_mint, mint_to},
        state::{Account, Mint},
    },
    spl_token_metadata::{
        instruction::{
            create_master_edition, create_metadata_accounts,
            mint_new_edition_from_master_edition_via_token, puff_metadata_account,
            update_metadata_accounts,
        },
        state::{
            get_reservation_list, Creator, Data, Edition, Key, MasterEditionV1, MasterEditionV2,
            Metadata, EDITION, MAX_NAME_LENGTH, MAX_SYMBOL_LENGTH, MAX_URI_LENGTH, PREFIX,
        },
    },
    std::str::FromStr,
};

const TOKEN_PROGRAM_PUBKEY: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
fn puff_unpuffed_metadata(_app_matches: &ArgMatches, payer: Keypair, client: RpcClient) {
    let metadata_accounts = client
        .get_program_accounts(&spl_token_metadata::id())
        .unwrap();
    let mut needing_puffing = vec![];
    for acct in metadata_accounts {
        if acct.1.data[0] == Key::MetadataV1 as u8 {
            match try_from_slice_unchecked(&acct.1.data) {
                Ok(val) => {
                    let account: Metadata = val;
                    if account.data.name.len() < MAX_NAME_LENGTH
                        || account.data.uri.len() < MAX_URI_LENGTH
                        || account.data.symbol.len() < MAX_SYMBOL_LENGTH
                        || account.edition_nonce.is_none()
                    {
                        needing_puffing.push(acct.0);
                    }
                }
                Err(_) => {
                    println!("Skipping {}", acct.0)
                }
            };
        }
    }
    println!("Found {} accounts needing puffing", needing_puffing.len());

    let mut instructions = vec![];
    let mut i = 0;
    while i < needing_puffing.len() {
        let pubkey = needing_puffing[i];
        instructions.push(puff_metadata_account(spl_token_metadata::id(), pubkey));
        if instructions.len() >= 20 {
            let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
            let recent_blockhash = client.get_recent_blockhash().unwrap().0;

            transaction.sign(&[&payer], recent_blockhash);
            match client.send_and_confirm_transaction(&transaction) {
                Ok(_) => {
                    println!("Another 20 down. At {} / {}", i, needing_puffing.len());
                    instructions = vec![];
                    i += 1;
                }
                Err(_) => {
                    println!("Txn failed. Retry.");
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
            }
        } else {
            i += 1;
        }
    }

    if instructions.len() > 0 {
        let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
        let recent_blockhash = client.get_recent_blockhash().unwrap().0;
        transaction.sign(&[&payer], recent_blockhash);
        client.send_and_confirm_transaction(&transaction).unwrap();
    }
}

fn mint_coins(app_matches: &ArgMatches, payer: Keypair, client: RpcClient) {
    let token_key = Pubkey::from_str(TOKEN_PROGRAM_PUBKEY).unwrap();
    let amount = match app_matches.value_of("amount") {
        Some(val) => Some(val.parse::<u64>().unwrap()),
        None => None,
    }
    .unwrap();
    let mint_key = pubkey_of(app_matches, "mint").unwrap();
    let mut instructions = vec![];

    let mut signers = vec![&payer];
    let destination_key: Pubkey;
    let destination = Keypair::new();
    if app_matches.is_present("destination") {
        destination_key = pubkey_of(app_matches, "destination").unwrap();
    } else {
        destination_key = destination.pubkey();
        signers.push(&destination);
        instructions.push(create_account(
            &payer.pubkey(),
            &destination_key,
            client
                .get_minimum_balance_for_rent_exemption(Account::LEN)
                .unwrap(),
            Account::LEN as u64,
            &token_key,
        ));
        instructions.push(
            initialize_account(&token_key, &destination_key, &mint_key, &payer.pubkey()).unwrap(),
        );
    }
    instructions.push(
        mint_to(
            &token_key,
            &mint_key,
            &destination_key,
            &payer.pubkey(),
            &[&payer.pubkey()],
            amount,
        )
        .unwrap(),
    );
    let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
    let recent_blockhash = client.get_recent_blockhash().unwrap().0;

    transaction.sign(&signers, recent_blockhash);
    client.send_and_confirm_transaction(&transaction).unwrap();

    println!("Minted {:?} tokens to {:?}.", amount, destination_key);
}
fn show_reservation_list(app_matches: &ArgMatches, _payer: Keypair, client: RpcClient) {
    let key = pubkey_of(app_matches, "key").unwrap();
    let mut res_data = client.get_account(&key).unwrap();
    let mut lamports = 0;
    let account_info = AccountInfo::new(
        &key,
        false,
        false,
        &mut lamports,
        &mut res_data.data,
        &res_data.owner,
        false,
        0,
    );

    let res_list = get_reservation_list(&account_info).unwrap();
    println!("Res list {:?}", res_list.reservations());
    println!(
        "current res spots: {:?}",
        res_list.current_reservation_spots()
    );
    println!("total res spots: {:?}", res_list.total_reservation_spots());
    println!("supply snapshot: {:?}", res_list.supply_snapshot());
}

fn show(app_matches: &ArgMatches, _payer: Keypair, client: RpcClient) {
    let program_key = spl_token_metadata::id();

    let printing_mint_key = pubkey_of(app_matches, "mint").unwrap();
    let master_metadata_seeds = &[
        PREFIX.as_bytes(),
        &program_key.as_ref(),
        printing_mint_key.as_ref(),
    ];
    let (master_metadata_key, _) =
        Pubkey::find_program_address(master_metadata_seeds, &program_key);

    let master_metadata_account = client.get_account(&master_metadata_key).unwrap();
    let master_metadata: Metadata =
        try_from_slice_unchecked(&master_metadata_account.data).unwrap();

    let update_authority = master_metadata.update_authority;

    let master_edition_seeds = &[
        PREFIX.as_bytes(),
        &program_key.as_ref(),
        &master_metadata.mint.as_ref(),
        EDITION.as_bytes(),
    ];
    let (master_edition_key, _) = Pubkey::find_program_address(master_edition_seeds, &program_key);
    let master_edition_account_res = client.get_account(&master_edition_key);

    println!("Metadata key: {:?}", master_metadata_key);
    println!("Metadata: {:#?}", master_metadata);
    println!("Update authority: {:?}", update_authority);
    match master_edition_account_res {
        Ok(master_edition_account) => {
            if master_edition_account.data[0] == Key::MasterEditionV1 as u8 {
                let master_edition: MasterEditionV1 =
                    try_from_slice_unchecked(&master_edition_account.data).unwrap();
                println!("Deprecated Master edition {:#?}", master_edition);
            } else if master_edition_account.data[0] == Key::MasterEditionV2 as u8 {
                let master_edition: MasterEditionV2 =
                    try_from_slice_unchecked(&master_edition_account.data).unwrap();
                println!("Master edition {:#?}", master_edition);
            } else {
                let edition: Edition =
                    try_from_slice_unchecked(&master_edition_account.data).unwrap();
                println!("Limited edition {:#?}", edition);
            }
        }
        Err(_) => {
            println!("No master edition or edition detected")
        }
    }
}

fn mint_edition_via_token_call(
    app_matches: &ArgMatches,
    payer: Keypair,
    client: RpcClient,
) -> (Edition, Pubkey, Pubkey) {
    let account_authority = read_keypair_file(
        app_matches
            .value_of("account_authority")
            .unwrap_or_else(|| app_matches.value_of("keypair").unwrap()),
    )
    .unwrap();

    let program_key = spl_token_metadata::id();
    let token_key = Pubkey::from_str(TOKEN_PROGRAM_PUBKEY).unwrap();

    let mint_key = pubkey_of(app_matches, "mint").unwrap();
    let existing_token_account = Pubkey::from_str(
        &client
            .get_token_accounts_by_owner(
                &account_authority.pubkey(),
                TokenAccountsFilter::Mint(mint_key),
            )
            .unwrap()
            .iter()
            .find(|x| {
                client
                    .get_token_account_balance(&Pubkey::from_str(&x.pubkey).unwrap())
                    .unwrap()
                    .amount
                    != "0"
            })
            .unwrap()
            .pubkey,
    )
    .unwrap();

    let new_mint_key = Keypair::new();
    let added_token_account = Keypair::new();
    let new_mint_pub = new_mint_key.pubkey();
    let metadata_seeds = &[
        PREFIX.as_bytes(),
        &program_key.as_ref(),
        &new_mint_pub.as_ref(),
    ];
    let (metadata_key, _) = Pubkey::find_program_address(metadata_seeds, &program_key);

    let edition_seeds = &[
        PREFIX.as_bytes(),
        &program_key.as_ref(),
        &new_mint_pub.as_ref(),
        EDITION.as_bytes(),
    ];
    let (edition_key, _) = Pubkey::find_program_address(edition_seeds, &program_key);

    let master_metadata_seeds = &[PREFIX.as_bytes(), &program_key.as_ref(), mint_key.as_ref()];
    let (master_metadata_key, _) =
        Pubkey::find_program_address(master_metadata_seeds, &program_key);

    let master_metadata_account = client.get_account(&master_metadata_key).unwrap();
    let master_metadata: Metadata =
        try_from_slice_unchecked(&master_metadata_account.data).unwrap();

    let master_edition_seeds = &[
        PREFIX.as_bytes(),
        &program_key.as_ref(),
        &master_metadata.mint.as_ref(),
        EDITION.as_bytes(),
    ];
    let (master_edition_key, _) = Pubkey::find_program_address(master_edition_seeds, &program_key);
    let master_edition_account = client.get_account(&master_edition_key).unwrap();
    let master_edition: MasterEditionV2 =
        try_from_slice_unchecked(&master_edition_account.data).unwrap();
    let signers = vec![&account_authority, &new_mint_key, &added_token_account];
    let mut instructions = vec![
        create_account(
            &payer.pubkey(),
            &new_mint_key.pubkey(),
            client
                .get_minimum_balance_for_rent_exemption(Mint::LEN)
                .unwrap(),
            Mint::LEN as u64,
            &token_key,
        ),
        initialize_mint(
            &token_key,
            &new_mint_key.pubkey(),
            &payer.pubkey(),
            Some(&payer.pubkey()),
            0,
        )
        .unwrap(),
        create_account(
            &payer.pubkey(),
            &added_token_account.pubkey(),
            client
                .get_minimum_balance_for_rent_exemption(Account::LEN)
                .unwrap(),
            Account::LEN as u64,
            &token_key,
        ),
        initialize_account(
            &token_key,
            &added_token_account.pubkey(),
            &new_mint_key.pubkey(),
            &payer.pubkey(),
        )
        .unwrap(),
        mint_to(
            &token_key,
            &new_mint_key.pubkey(),
            &added_token_account.pubkey(),
            &payer.pubkey(),
            &[&payer.pubkey()],
            1,
        )
        .unwrap(),
    ];

    instructions.push(mint_new_edition_from_master_edition_via_token(
        program_key,
        metadata_key,
        edition_key,
        master_edition_key,
        new_mint_key.pubkey(),
        account_authority.pubkey(),
        payer.pubkey(),
        account_authority.pubkey(),
        existing_token_account,
        account_authority.pubkey(),
        master_metadata_key,
        master_metadata.mint,
        master_edition.supply + 1,
    ));

    let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
    let recent_blockhash = client.get_recent_blockhash().unwrap().0;

    transaction.sign(&signers, recent_blockhash);
    client.send_and_confirm_transaction(&transaction).unwrap();
    let account = client.get_account(&edition_key).unwrap();
    let edition: Edition = try_from_slice_unchecked(&account.data).unwrap();
    (edition, edition_key, new_mint_key.pubkey())
}

fn master_edition_call(
    app_matches: &ArgMatches,
    payer: Keypair,
    client: RpcClient,
) -> (MasterEditionV2, Pubkey) {
    let update_authority = read_keypair_file(
        app_matches
            .value_of("update_authority")
            .unwrap_or_else(|| app_matches.value_of("keypair").unwrap()),
    )
    .unwrap();
    let mint_authority = read_keypair_file(
        app_matches
            .value_of("mint_authority")
            .unwrap_or_else(|| app_matches.value_of("keypair").unwrap()),
    )
    .unwrap();

    let program_key = spl_token_metadata::id();
    let token_key = Pubkey::from_str(TOKEN_PROGRAM_PUBKEY).unwrap();

    let mint_key = pubkey_of(app_matches, "mint").unwrap();
    let metadata_seeds = &[PREFIX.as_bytes(), &program_key.as_ref(), mint_key.as_ref()];
    let (metadata_key, _) = Pubkey::find_program_address(metadata_seeds, &program_key);

    let metadata_account = client.get_account(&metadata_key).unwrap();
    let metadata: Metadata = try_from_slice_unchecked(&metadata_account.data).unwrap();

    let master_edition_seeds = &[
        PREFIX.as_bytes(),
        &program_key.as_ref(),
        &metadata.mint.as_ref(),
        EDITION.as_bytes(),
    ];
    let (master_edition_key, _) = Pubkey::find_program_address(master_edition_seeds, &program_key);

    let max_supply = match app_matches.value_of("max_supply") {
        Some(val) => Some(val.parse::<u64>().unwrap()),
        None => None,
    };

    let added_token_account = Keypair::new();

    let needs_a_token = app_matches.is_present("add_one_token");
    let mut signers = vec![&update_authority, &mint_authority];
    let mut instructions = vec![];

    if needs_a_token {
        signers.push(&added_token_account);
        instructions.push(create_account(
            &payer.pubkey(),
            &added_token_account.pubkey(),
            client
                .get_minimum_balance_for_rent_exemption(Account::LEN)
                .unwrap(),
            Account::LEN as u64,
            &token_key,
        ));
        instructions.push(
            initialize_account(
                &token_key,
                &added_token_account.pubkey(),
                &metadata.mint,
                &payer.pubkey(),
            )
            .unwrap(),
        );
        instructions.push(
            mint_to(
                &token_key,
                &metadata.mint,
                &added_token_account.pubkey(),
                &payer.pubkey(),
                &[&payer.pubkey()],
                1,
            )
            .unwrap(),
        )
    }

    instructions.push(create_master_edition(
        program_key,
        master_edition_key,
        mint_key,
        update_authority.pubkey(),
        mint_authority.pubkey(),
        metadata_key,
        payer.pubkey(),
        max_supply,
    ));

    let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
    let recent_blockhash = client.get_recent_blockhash().unwrap().0;

    transaction.sign(&signers, recent_blockhash);
    client.send_and_confirm_transaction(&transaction).unwrap();
    let account = client.get_account(&master_edition_key).unwrap();
    let master_edition: MasterEditionV2 = try_from_slice_unchecked(&account.data).unwrap();
    (master_edition, master_edition_key)
}

fn update_metadata_account_call(
    app_matches: &ArgMatches,
    payer: Keypair,
    client: RpcClient,
) -> (Metadata, Pubkey) {
    let update_authority = read_keypair_file(
        app_matches
            .value_of("update_authority")
            .unwrap_or_else(|| app_matches.value_of("keypair").unwrap()),
    )
    .unwrap();
    let program_key = spl_token_metadata::id();
    let mint_key = pubkey_of(app_matches, "mint").unwrap();
    let metadata_seeds = &[PREFIX.as_bytes(), &program_key.as_ref(), mint_key.as_ref()];
    let (metadata_key, _) = Pubkey::find_program_address(metadata_seeds, &program_key);

    let uri = match app_matches.value_of("uri") {
        Some(val) => Some(val.to_owned()),
        None => None,
    };

    let name = match app_matches.value_of("name") {
        Some(val) => Some(val.to_owned()),
        None => None,
    };

    let new_update_authority = pubkey_of(app_matches, "new_update_authority");

    let metadata_account = client.get_account(&metadata_key).unwrap();
    let metadata: Metadata = try_from_slice_unchecked(&metadata_account.data).unwrap();

    let new_data = Data {
        name: name.unwrap_or(metadata.data.name),
        symbol: metadata.data.symbol,
        uri: uri.unwrap_or(metadata.data.uri),
        seller_fee_basis_points: 0,
        creators: metadata.data.creators,
    };

    let instructions = [update_metadata_accounts(
        program_key,
        metadata_key,
        update_authority.pubkey(),
        new_update_authority,
        Some(new_data),
        None,
    )];

    let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
    let recent_blockhash = client.get_recent_blockhash().unwrap().0;
    let signers = vec![&update_authority];

    transaction.sign(&signers, recent_blockhash);
    client.send_and_confirm_transaction(&transaction).unwrap();
    let metadata_account = client.get_account(&metadata_key).unwrap();
    let metadata: Metadata = try_from_slice_unchecked(&metadata_account.data).unwrap();
    (metadata, metadata_key)
}

fn pull_llama_arweave_uris(app_matches: &ArgMatches, payer: Keypair, client: RpcClient) {
    let mut file = File::open("all_metadata.json").unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    let keys: Vec<String> = serde_json::from_str(&contents).unwrap();
    let mut uris: Vec<(String, Option<String>, String)> = vec![];
    let mut i = 0;
    let len = keys.len();
    let start = app_matches
        .value_of("start")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let end = app_matches
        .value_of("end")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    for key in keys {
        if i >= start && i < end {
            println!("Doing {} out of {}", i, len);
            let metadata_account = client
                .get_account(&Pubkey::from_str(&key).unwrap())
                .unwrap();
            let metadata: Metadata = try_from_slice_unchecked(&metadata_account.data).unwrap();
            match reqwest::blocking::get(&metadata.data.uri) {
                Ok(mut res) => {
                    let mut body = String::new();
                    let mut uri_body = None;
                    match res.read_to_string(&mut body) {
                        Ok(_) => uri_body = Some(body),
                        Err(_) => {
                            println!("Arweave URL {} does not exist", &metadata.data.uri)
                        }
                    };
                    uris.push((metadata.data.uri.replace("\u{0000}", ""), uri_body, key));
                }
                Err(_) => uris.push((metadata.data.uri.replace("\u{0000}", ""), None, key)),
            }
        }
        i += 1;
    }

    let mut file = File::create(
        "metadata_uris_".to_owned() + &start.to_string() + "_" + &end.to_string() + ".json",
    )
    .unwrap();

    file.write_all(serde_json::to_string(&uris).unwrap().as_bytes())
        .unwrap();
}

fn airdrop(app_matches: &ArgMatches, payer: Keypair, client: RpcClient) {
    let update_authority = read_keypair_file(
        app_matches
            .value_of("update_authority")
            .unwrap_or_else(|| app_matches.value_of("keypair").unwrap()),
    )
    .unwrap();

    let metadata_program = spl_token_metadata::id();

    let participation_trophy =
        Pubkey::from_str("Gt2VHnTpWhczM2EvYQSVAf3BHCVNyR1q5yUGibzb6sEX").unwrap();

    let metadata_seeds = &[
        PREFIX.as_bytes(),
        &metadata_program.as_ref(),
        &participation_trophy.as_ref(),
    ];
    let (master_metadata_key, _) = Pubkey::find_program_address(metadata_seeds, &metadata_program);
    let master_metadata_account = client.get_account(&master_metadata_key).unwrap();
    let master_metadata: Metadata =
        try_from_slice_unchecked(&master_metadata_account.data).unwrap();

    let master_edition_seeds = &[
        PREFIX.as_bytes(),
        &metadata_program.as_ref(),
        &master_metadata.mint.as_ref(),
        EDITION.as_bytes(),
    ];
    let (master_edition_key, _) =
        Pubkey::find_program_address(master_edition_seeds, &metadata_program);
    let master_edition_account = client.get_account(&master_edition_key).unwrap();
    let master_edition: MasterEditionV2 =
        try_from_slice_unchecked(&master_edition_account.data).unwrap();
    let edition_offset = master_edition.supply;
    let existing_token_account = Pubkey::from_str(
        &client
            .get_token_accounts_by_owner(
                &payer.pubkey(),
                TokenAccountsFilter::Mint(participation_trophy),
            )
            .unwrap()
            .iter()
            .find(|x| {
                client
                    .get_token_account_balance(&Pubkey::from_str(&x.pubkey).unwrap())
                    .unwrap()
                    .amount
                    != "0"
            })
            .unwrap()
            .pubkey,
    )
    .unwrap();

    let mut file = File::open(app_matches.value_of("file").unwrap()).unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    let keys: Vec<(String, u8)> = serde_json::from_str(&contents).unwrap();

    /* let mut file = File::open(app_matches.value_of("cache").unwrap()).unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    let cache_keys: Vec<(String, u8)> = serde_json::from_str(&contents).unwrap();*/
    let token_key = spl_token::id();
    let len = keys.len();
    let mut i = 0;
    while i < len {
        println!("At {} out of {}", i, len);
        let key = &keys[i];
        let mut j: usize = 0;
        /*if j < cache_keys.len() {
            j = cache_keys[i].1 as usize;
        }*/
        while j < key.1.into() {
            let mut signers = vec![&update_authority];
            let mut instructions = vec![];

            let new_mint_key = Keypair::new();
            let added_token_account = Keypair::new();
            let new_mint_pub = new_mint_key.pubkey();

            println!("Granting nft {} to key {}", j, key.0);

            let metadata_seeds = &[
                PREFIX.as_bytes(),
                &metadata_program.as_ref(),
                &new_mint_pub.as_ref(),
            ];
            let (new_metadata_key, _) =
                Pubkey::find_program_address(metadata_seeds, &metadata_program);

            let edition_seeds = &[
                PREFIX.as_bytes(),
                &metadata_program.as_ref(),
                &new_mint_pub.as_ref(),
                EDITION.as_bytes(),
            ];
            let (edition_key, _) = Pubkey::find_program_address(edition_seeds, &metadata_program);

            signers.push(&new_mint_key);
            signers.push(&added_token_account);
            instructions.push(create_account(
                &payer.pubkey(),
                &new_mint_key.pubkey(),
                client
                    .get_minimum_balance_for_rent_exemption(Mint::LEN)
                    .unwrap(),
                Mint::LEN as u64,
                &token_key,
            ));
            instructions.push(
                initialize_mint(
                    &token_key,
                    &new_mint_key.pubkey(),
                    &payer.pubkey(),
                    Some(&payer.pubkey()),
                    0,
                )
                .unwrap(),
            );
            instructions.push(create_account(
                &payer.pubkey(),
                &added_token_account.pubkey(),
                client
                    .get_minimum_balance_for_rent_exemption(Account::LEN)
                    .unwrap(),
                Account::LEN as u64,
                &token_key,
            ));

            instructions.push(
                initialize_account(
                    &token_key,
                    &added_token_account.pubkey(),
                    &new_mint_key.pubkey(),
                    &Pubkey::from_str(&key.0).unwrap(),
                )
                .unwrap(),
            );
            instructions.push(
                mint_to(
                    &token_key,
                    &new_mint_key.pubkey(),
                    &added_token_account.pubkey(),
                    &payer.pubkey(),
                    &[&payer.pubkey()],
                    1,
                )
                .unwrap(),
            );

            instructions.push(mint_new_edition_from_master_edition_via_token(
                metadata_program,
                new_metadata_key,
                edition_key,
                master_edition_key,
                new_mint_key.pubkey(),
                payer.pubkey(),
                payer.pubkey(),
                payer.pubkey(),
                existing_token_account,
                Pubkey::from_str(&key.0).unwrap(),
                master_metadata_key,
                master_metadata.mint,
                edition_offset + i as u64 + j as u64 + 1,
            ));

            let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
            let recent_blockhash = client.get_recent_blockhash().unwrap().0;

            transaction.sign(&signers, recent_blockhash);
            match client.send_transaction(&transaction) {
                Ok(_) => j += 1,
                Err(err) => {
                    println!("Transaction failed. No retry! {:?}", err);
                    j += 1
                }
            }
        }
        i += 1
    }
}

fn find_all_llamas(app_matches: &ArgMatches, payer: Keypair, client: RpcClient) {
    let llama_key = Pubkey::from_str("LLAmArGWBCspEarLTCBpKLdXxYS4EUuiQZQmy1RD8oc").unwrap();
    let start = app_matches
        .value_of("start")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let end = app_matches
        .value_of("end")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let token_accounts = client
        .get_token_accounts_by_owner(&llama_key, TokenAccountsFilter::ProgramId(spl_token::id()))
        .unwrap();

    let mut bad_metadata: Vec<(Value, String)> = vec![];
    let metadata_program = spl_token_metadata::id();
    let mut i = 0;
    let len = token_accounts.len();
    for account in token_accounts {
        if i >= start && i < end {
            println!("At {} out of {}", i, len);
            let actual_data = client
                .get_account(&Pubkey::from_str(&account.pubkey).unwrap())
                .unwrap();
            let token_account = Account::unpack_unchecked(&actual_data.data).unwrap();
            let metadata_seeds = &[
                PREFIX.as_bytes(),
                &metadata_program.as_ref(),
                token_account.mint.as_ref(),
            ];
            let (metadata_key, _) = Pubkey::find_program_address(metadata_seeds, &metadata_program);
            match client.get_account(&metadata_key) {
                Ok(val) => {
                    let md: Metadata = try_from_slice_unchecked(val.data()).unwrap();
                    let mut res = reqwest::blocking::get(md.data.uri).unwrap();
                    let mut body = String::new();
                    res.read_to_string(&mut body).unwrap();
                    let parsed: Value = serde_json::from_str(&body).unwrap();
                    let mut found = md.data.name == "Tuco the Ugly";
                    if let Some(arr) = parsed["attributes"].as_array() {
                        for attribute in arr {
                            if attribute["trait_type"] == "Alive" {
                                found = true;
                                break;
                            }
                        }
                    }
                    if !found {
                        println!("Found a bad one! {}", metadata_key);
                        bad_metadata.push((parsed, metadata_key.to_string()))
                    }
                }
                Err(_) => {
                    println!("token account {} does not have a metadata", account.pubkey)
                }
            }
        }
        i += 1;
    }

    let mut file = File::create(
        "bad_metadata_".to_owned() + &start.to_string() + "_" + &end.to_string() + ".json",
    )
    .unwrap();

    file.write_all(serde_json::to_string(&bad_metadata).unwrap().as_bytes())
        .unwrap();
}

fn create_new_llamas(app_matches: &ArgMatches, payer: Keypair, client: RpcClient) {
    let start = app_matches
        .value_of("start")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let end = app_matches
        .value_of("end")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let mut file = File::open("llamas_new.json").unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    let keys: Vec<(String, Value)> = serde_json::from_str(&contents).unwrap();
    let mut file = File::open("prints.json").unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    let wallets: Vec<String> = serde_json::from_str(&contents).unwrap();
    let token_key = spl_token::id();
    let len = wallets.len();
    let mut i = 0;
    while i < len {
        if i >= start && i < end {
            println!("At {} out of {}", i, len);
            let arweave_manifest = &keys[i].0;
            let arweave: &Value = &keys[i].1;
            let wallet = &Pubkey::from_str(&wallets[i]).unwrap();

            let program_key = spl_token_metadata::id();
            let token_key = Pubkey::from_str(TOKEN_PROGRAM_PUBKEY).unwrap();
            let name = arweave["name"].to_owned();
            let symbol = arweave["symbol"].to_owned();
            let uri = arweave_manifest;
            let mutable = true;
            let new_mint = Keypair::new();
            let mint_key = new_mint.pubkey();
            let metadata_seeds = &[PREFIX.as_bytes(), &program_key.as_ref(), mint_key.as_ref()];
            let (metadata_key, _) = Pubkey::find_program_address(metadata_seeds, &program_key);
            let mut signers = vec![&payer];

            let edition_seeds = &[
                PREFIX.as_bytes(),
                &program_key.as_ref(),
                &mint_key.as_ref(),
                EDITION.as_bytes(),
            ];
            let (edition_key, _) = Pubkey::find_program_address(edition_seeds, &program_key);

            let mut new_mint_instructions = vec![
                create_account(
                    &payer.pubkey(),
                    &mint_key,
                    client
                        .get_minimum_balance_for_rent_exemption(Mint::LEN)
                        .unwrap(),
                    Mint::LEN as u64,
                    &token_key,
                ),
                initialize_mint(
                    &token_key,
                    &mint_key,
                    &payer.pubkey(),
                    Some(&payer.pubkey()),
                    0,
                )
                .unwrap(),
            ];
            let mut instructions = vec![];

            let new_metadata_instruction = create_metadata_accounts(
                program_key,
                metadata_key,
                mint_key,
                payer.pubkey(),
                payer.pubkey(),
                payer.pubkey(),
                name.to_string(),
                symbol.to_string(),
                uri.to_string(),
                Some(vec![Creator {
                    address: Pubkey::from_str("LamapQPXuMYEuvsyZqK2UPqn1XCT2sW1soURj7ZJkZF")
                        .unwrap(),
                    verified: true,
                    share: 100,
                }]),
                500,
                true,
                mutable,
            );

            instructions.append(&mut new_mint_instructions);
            instructions.push(new_metadata_instruction);

            let added_token_account = Keypair::new();
            signers.push(&added_token_account);
            instructions.push(create_account(
                &payer.pubkey(),
                &added_token_account.pubkey(),
                client
                    .get_minimum_balance_for_rent_exemption(Account::LEN)
                    .unwrap(),
                Account::LEN as u64,
                &token_key,
            ));
            instructions.push(
                initialize_account(
                    &token_key,
                    &added_token_account.pubkey(),
                    &mint_key,
                    &wallet,
                )
                .unwrap(),
            );
            instructions.push(
                mint_to(
                    &token_key,
                    &mint_key,
                    &added_token_account.pubkey(),
                    &payer.pubkey(),
                    &[&payer.pubkey()],
                    1,
                )
                .unwrap(),
            );

            instructions.push(create_master_edition(
                program_key,
                edition_key,
                mint_key,
                payer.pubkey(),
                payer.pubkey(),
                metadata_key,
                payer.pubkey(),
                Some(0u64),
            ));

            let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
            let recent_blockhash = client.get_recent_blockhash().unwrap().0;
            signers.push(&new_mint);

            transaction.sign(&signers, recent_blockhash);
            match client.send_and_confirm_transaction(&transaction) {
                Ok(_) => {
                    i += 1;
                }
                Err(err) => {
                    println!("Transaction failed. Retry {:?}", err);
                }
            }
        } else {
            i += 1;
        }
    }
}

fn update_new_llamas(app_matches: &ArgMatches, payer: Keypair, client: RpcClient) {
    let update_authority = read_keypair_file(
        app_matches
            .value_of("update_authority")
            .unwrap_or_else(|| app_matches.value_of("keypair").unwrap()),
    )
    .unwrap();
    let start = app_matches
        .value_of("start")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let end = app_matches
        .value_of("end")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let metadata_program = spl_token_metadata::id();

    let mut file = File::open(app_matches.value_of("file").unwrap()).unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    let keys: Vec<(String, String)> = serde_json::from_str(&contents).unwrap();

    let mut old_file = File::open(app_matches.value_of("old_file").unwrap()).unwrap();
    let mut old_contents = String::new();
    old_file.read_to_string(&mut old_contents).unwrap();
    let old_keys: Vec<(String, String)> = serde_json::from_str(&old_contents).unwrap();

    let len = keys.len();
    let mut i = 0;

    let mut saved = vec![];
    while i < len {
        if i >= start && i < end {
            println!("At {} out of {}", i, len);
            let key = &keys[i];

            let arweave_uri = &key.1;
            let metadata_key = Pubkey::from_str(&key.0).unwrap();
            for n in &old_keys {
                if n.0 == key.0 {
                    i += 1;
                    println!("Skipping {} because already processed", key.0);
                    continue;
                }
            }
            let metadata_account = client.get_account(&metadata_key).unwrap();
            let metadata: Metadata = try_from_slice_unchecked(&metadata_account.data).unwrap();

            let new_data = Data {
                name: metadata.data.name.replace('"', ""),
                symbol: metadata.data.symbol,
                uri: arweave_uri.to_owned(),
                seller_fee_basis_points: metadata.data.seller_fee_basis_points,
                creators: metadata.data.creators,
            };

            let signers = vec![&update_authority];
            let instructions = vec![update_metadata_accounts(
                metadata_program,
                metadata_key,
                update_authority.pubkey(),
                None,
                Some(new_data),
                Some(true),
            )];

            let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
            let recent_blockhash = client.get_recent_blockhash().unwrap().0;

            transaction.sign(&signers, recent_blockhash);
            match client.send_transaction(&transaction) {
                Ok(_) => {
                    i += 1;
                    saved.push(metadata_key.to_string());
                }
                Err(err) => {
                    println!("Transaction failed. Retry {:?}", err);
                }
            }
        } else {
            i += 1;
        }
    }
    let saved_str = serde_json::to_string(&saved).unwrap();
    fs::write("saved_updates.json", saved_str).unwrap();
}

fn file_refund(app_matches: &ArgMatches, payer: Keypair, client: RpcClient) {
    let start = app_matches
        .value_of("start")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let end = app_matches
        .value_of("end")
        .unwrap()
        .parse::<usize>()
        .unwrap();

    let mut file = File::open(app_matches.value_of("file").unwrap()).unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    let keys: Vec<Value> = serde_json::from_str(&contents).unwrap();

    let mut i = 0;
    for key in keys {
        if i >= start && i < end {
            let instructions = [system_instruction::transfer(
                &payer.pubkey(),
                &Pubkey::from_str(key["pubkey"].as_str().unwrap()).unwrap(),
                key["amount"].as_u64().unwrap(),
            )];
            println!(
                "Paying {} lamports to {}",
                key["amount"].as_u64().unwrap(),
                key["pubkey"].as_str().unwrap()
            );
            let signers = [&payer];
            let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
            let recent_blockhash = client.get_recent_blockhash().unwrap().0;
            transaction.sign(&signers, recent_blockhash);
            client.send_and_confirm_transaction(&transaction).unwrap();
        }
        i += 1
    }
}

fn create_metadata_account_call(
    app_matches: &ArgMatches,
    payer: Keypair,
    client: RpcClient,
) -> (Metadata, Pubkey) {
    let update_authority = read_keypair_file(
        app_matches
            .value_of("update_authority")
            .unwrap_or_else(|| app_matches.value_of("keypair").unwrap()),
    )
    .unwrap();

    let program_key = spl_token_metadata::id();
    let token_key = Pubkey::from_str(TOKEN_PROGRAM_PUBKEY).unwrap();
    let name = app_matches.value_of("name").unwrap().to_owned();
    let symbol = app_matches.value_of("symbol").unwrap().to_owned();
    let uri = app_matches.value_of("uri").unwrap().to_owned();
    let create_new_mint = !app_matches.is_present("mint");
    let mutable = app_matches.is_present("mutable");
    let new_mint = Keypair::new();
    let mint_key = match app_matches.value_of("mint") {
        Some(_val) => pubkey_of(app_matches, "mint").unwrap(),
        None => new_mint.pubkey(),
    };
    let metadata_seeds = &[PREFIX.as_bytes(), &program_key.as_ref(), mint_key.as_ref()];
    let (metadata_key, _) = Pubkey::find_program_address(metadata_seeds, &program_key);

    let mut new_mint_instructions = vec![
        create_account(
            &payer.pubkey(),
            &mint_key,
            client
                .get_minimum_balance_for_rent_exemption(Mint::LEN)
                .unwrap(),
            Mint::LEN as u64,
            &token_key,
        ),
        initialize_mint(
            &token_key,
            &mint_key,
            &payer.pubkey(),
            Some(&payer.pubkey()),
            0,
        )
        .unwrap(),
    ];

    let new_metadata_instruction = create_metadata_accounts(
        program_key,
        metadata_key,
        mint_key,
        payer.pubkey(),
        payer.pubkey(),
        update_authority.pubkey(),
        name,
        symbol,
        uri,
        None,
        0,
        update_authority.pubkey() != payer.pubkey(),
        mutable,
    );

    let mut instructions = vec![];

    if create_new_mint {
        instructions.append(&mut new_mint_instructions)
    }

    instructions.push(new_metadata_instruction);

    let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
    let recent_blockhash = client.get_recent_blockhash().unwrap().0;
    let mut signers = vec![&payer];
    if create_new_mint {
        signers.push(&new_mint);
    }
    if update_authority.pubkey() != payer.pubkey() {
        signers.push(&update_authority)
    }
    transaction.sign(&signers, recent_blockhash);
    client.send_and_confirm_transaction(&transaction).unwrap();
    let account = client.get_account(&metadata_key).unwrap();
    let metadata: Metadata = try_from_slice_unchecked(&account.data).unwrap();
    (metadata, metadata_key)
}

fn main() {
    let app_matches = App::new(crate_name!())
        .about(crate_description!())
        .version(crate_version!())
        .arg(
            Arg::with_name("keypair")
                .long("keypair")
                .value_name("KEYPAIR")
                .validator(is_valid_signer)
                .takes_value(true)
                .global(true)
                .help("Filepath or URL to a keypair"),
        )
        .arg(
            Arg::with_name("json_rpc_url")
                .long("url")
                .value_name("URL")
                .takes_value(true)
                .global(true)
                .validator(is_url)
                .help("JSON RPC URL for the cluster [default: devnet]"),
        )
        .arg(
            Arg::with_name("update_authority")
                .long("update_authority")
                .value_name("UPDATE_AUTHORITY")
                .takes_value(true)
                .global(true)
                .help("Update authority filepath or url to keypair besides yourself, defaults to normal keypair"),
        )
        .subcommand(
     SubCommand::with_name("create_metadata_accounts")
                .about("Create Metadata Accounts")
                .arg(
                    Arg::with_name("name")
                        .long("name")
                        .global(true)
                        .value_name("NAME")
                        .takes_value(true)
                        .help("name for the Mint"),
                )
                .arg(
                    Arg::with_name("symbol")
                        .long("symbol")
                        .value_name("SYMBOL")
                        .takes_value(true)
                        .global(true)
                        .help("symbol for the Mint"),
                )
                .arg(
                    Arg::with_name("uri")
                        .long("uri")
                        .value_name("URI")
                        .takes_value(true)
                        .required(true)
                        .help("URI for the Mint"),
                )
                .arg(
                    Arg::with_name("mint")
                        .long("mint")
                        .value_name("MINT")
                        .takes_value(true)
                        .required(false)
                        .help("Pubkey for an existing mint (random new mint otherwise)"),
                )
                .arg(
                    Arg::with_name("mutable")
                        .long("mutable")
                        .value_name("MUTABLE")
                        .takes_value(false)
                        .required(false)
                        .help("Permit future metadata updates"),
                )
        ).subcommand(
            SubCommand::with_name("mint_coins")
                       .about("Mint coins to your mint to an account")
                       .arg(
                        Arg::with_name("mint")
                            .long("mint")
                            .value_name("MINT")
                            .required(true)
                            .validator(is_valid_pubkey)
                            .takes_value(true)
                            .help("Mint of the Metadata"),
                    ).arg(
                        Arg::with_name("destination")
                            .long("destination")
                            .value_name("DESTINATION")
                            .required(false)
                            .validator(is_valid_pubkey)
                            .takes_value(true)
                            .help("Destination account. If one isnt given, one is made."),
                    ).arg(
                        Arg::with_name("amount")
                            .long("amount")
                            .value_name("AMOUNT")
                            .required(true)
                            .takes_value(true)
                            .help("How many"),
                    )
               )
        .subcommand(
     SubCommand::with_name("update_metadata_accounts")
                .about("Update Metadata Accounts")
                .arg(
                    Arg::with_name("mint")
                        .long("mint")
                        .value_name("MINT")
                        .required(true)
                        .validator(is_valid_pubkey)
                        .takes_value(true)
                        .help("Mint of the Metadata"),
                )
                .arg(
                    Arg::with_name("uri")
                        .long("uri")
                        .value_name("URI")
                        .takes_value(true)
                        .required(false)
                        .help("new URI for the Metadata"),
                )
                .arg(
                    Arg::with_name("name")
                        .long("name")
                        .value_name("NAME")
                        .takes_value(true)
                        .required(false)
                        .help("new NAME for the Metadata"),
                )
                .arg(
                    Arg::with_name("new_update_authority")
                        .long("new_update_authority")
                        .value_name("NEW_UPDATE_AUTHORITY")
                        .required(false)
                        .validator(is_valid_pubkey)
                        .takes_value(true)
                        .help("New update authority"))
        ).subcommand(
            SubCommand::with_name("show")
                .about("Show")
                .arg(
                    Arg::with_name("mint")
                        .long("mint")
                        .value_name("MINT")
                        .required(true)
                        .validator(is_valid_pubkey)
                        .takes_value(true)
                        .help("Metadata mint"),
                )
        ).subcommand(
            SubCommand::with_name("show_reservation_list")
                .about("Show Reservation List")
                .arg(
                    Arg::with_name("key")
                        .long("key")
                        .value_name("KEY")
                        .required(true)
                        .validator(is_valid_pubkey)
                        .takes_value(true)
                        .help("Account key of reservation list"),
                )
        )
        .subcommand(
            SubCommand::with_name("create_master_edition")
                .about("Create Master Edition out of Metadata")
                .arg(
                    Arg::with_name("add_one_token")
                        .long("add_one_token")
                        .value_name("ADD_ONE_TOKEN")
                        .required(false)
                        .takes_value(false)
                        .help("Add a token to this mint before calling (useful if your mint has zero tokens, this action requires one to be present)"),
                ).arg(
                    Arg::with_name("max_supply")
                        .long("max_supply")
                        .value_name("MAX_SUPPLY")
                        .required(false)
                        .takes_value(true)
                        .help("Set a maximum supply that can be minted."),
                ).arg(
                    Arg::with_name("mint")
                        .long("mint")
                        .value_name("MINT")
                        .required(true)
                        .validator(is_valid_pubkey)
                        .takes_value(true)
                        .help("Metadata mint to from which to create a master edition."),
                ).arg(
                    Arg::with_name("mint_authority")
                        .long("mint_authority")
                        .value_name("MINT_AUTHORITY")
                        .validator(is_valid_signer)
                        .takes_value(true)
                        .required(false)
                        .help("Filepath or URL to a keypair representing mint authority, defaults to you"),
                )
        ).subcommand(
                SubCommand::with_name("mint_new_edition_from_master_edition_via_token")
                        .about("Mint new edition from master edition via a token - this will just also mint the token for you and submit it.")
                        .arg(
                            Arg::with_name("mint")
                                .long("mint")
                                .value_name("MINT")
                                .required(true)
                                .validator(is_valid_pubkey)
                                .takes_value(true)
                                .help("Metadata Mint from which to mint this new edition"),
                        ).arg(
                            Arg::with_name("account")
                                .long("account")
                                .value_name("ACCOUNT")
                                .required(false)
                                .validator(is_valid_pubkey)
                                .takes_value(true)
                                .help("Account which contains authorization token. If not provided, one will be made."),
                        ).arg(
                            Arg::with_name("account_authority")
                                .long("account_authority")
                                .value_name("ACCOUNT_AUTHORITY")
                                .required(false)
                                .validator(is_valid_signer)
                                .takes_value(true)
                                .help("Account's authority, defaults to you"),
                        )

        ).subcommand(
                SubCommand::with_name("puff_unpuffed_metadata")
                        .about("Take metadata that still have variable length name, symbol, and uri fields and stretch them out with null symbols so they can be searched more easily by RPC.")
        ).subcommand(
                SubCommand::with_name("find_all_llamas").arg(
                    Arg::with_name("start")
                        .long("start")
                        .value_name("START")
                        .takes_value(true)
                        .required(true)
                        .help("start"),
                ).arg(
                    Arg::with_name("end")
                        .long("end")
                        .value_name("END")
                        .takes_value(true)
                        .required(true)
                        .help("end"),
                )
                        .about("")
        ).subcommand(
            SubCommand::with_name("airdrop").arg(
                Arg::with_name("file")
                    .long("file")
                    .value_name("FILE")
                    .takes_value(true)
                    .required(true)
                    .help("file"),
            )
                    .about("")
    ).subcommand(
            SubCommand::with_name("pull_llama_arweave_uris").arg(
                Arg::with_name("start")
                    .long("start")
                    .value_name("START")
                    .takes_value(true)
                    .required(true)
                    .help("start"),
            ).arg(
                Arg::with_name("end")
                    .long("end")
                    .value_name("END")
                    .takes_value(true)
                    .required(true)
                    .help("end"),
            )
                    .about(""))
                    .subcommand(
                        SubCommand::with_name("create_new_llamas").arg(
                            Arg::with_name("start")
                                .long("start")
                                .value_name("START")
                                .takes_value(true)
                                .required(true)
                                .help("start"),
                        ).arg(
                            Arg::with_name("end")
                                .long("end")
                                .value_name("END")
                                .takes_value(true)
                                .required(true)
                                .help("end"),
                        ))
                        .subcommand(
                            SubCommand::with_name("update_new_llamas").arg(
                                Arg::with_name("file")
                                    .long("file")
                                    .value_name("FILE")
                                    .takes_value(true)
                                    .required(true)
                                    .help("file"),
                            ).arg(
                                Arg::with_name("old_file")
                                    .long("old_file")
                                    .value_name("OLD_FILE")
                                    .takes_value(true)
                                    .required(true)
                                    .help("old_file"),
                            ).arg(
                                Arg::with_name("start")
                                    .long("start")
                                    .value_name("START")
                                    .takes_value(true)
                                    .required(true)
                                    .help("start"),
                            ).arg(
                                Arg::with_name("end")
                                    .long("end")
                                    .value_name("END")
                                    .takes_value(true)
                                    .required(true)
                                    .help("end"),
                            ))
                            .subcommand(
                                SubCommand::with_name("file_refunds").arg(
                                    Arg::with_name("file")
                                        .long("file")
                                        .value_name("FILE")
                                        .takes_value(true)
                                        .required(true)
                                        .help("file"),
                                ).arg(
                                    Arg::with_name("start")
                                        .long("start")
                                        .value_name("START")
                                        .takes_value(true)
                                        .required(true)
                                        .help("start"),
                                ).arg(
                                    Arg::with_name("end")
                                        .long("end")
                                        .value_name("END")
                                        .takes_value(true)
                                        .required(true)
                                        .help("end"),
                                )).get_matches();

    let client = RpcClient::new(
        app_matches
            .value_of("json_rpc_url")
            .unwrap_or(&"https://api.devnet.solana.com".to_owned())
            .to_owned(),
    );

    let payer = read_keypair_file(app_matches.value_of("keypair").unwrap()).unwrap();

    let (sub_command, sub_matches) = app_matches.subcommand();
    match (sub_command, sub_matches) {
        ("create_metadata_accounts", Some(arg_matches)) => {
            let (metadata, metadata_key) = create_metadata_account_call(arg_matches, payer, client);
            println!(
                "Create metadata account with mint {:?} and key {:?} and name of {:?} and symbol of {:?}",
                metadata.mint, metadata_key, metadata.data.name, metadata.data.symbol
            );
        }
        ("update_metadata_accounts", Some(arg_matches)) => {
            let (metadata, metadata_key) = update_metadata_account_call(arg_matches, payer, client);
            println!(
                "Update metadata account with mint {:?} and key {:?} which now has URI of {:?}",
                metadata.mint, metadata_key, metadata.data.uri
            );
        }
        ("create_master_edition", Some(arg_matches)) => {
            let (master_edition, master_edition_key) =
                master_edition_call(arg_matches, payer, client);
            println!(
                "Created master edition {:?} with key {:?}",
                master_edition, master_edition_key
            );
        }
        ("mint_new_edition_from_master_edition_via_token", Some(arg_matches)) => {
            let (edition, edition_key, mint) =
                mint_edition_via_token_call(arg_matches, payer, client);
            println!(
                "New edition: {:?}\nParent edition: {:?}\nEdition number: {:?}\nToken mint: {:?}",
                edition_key, edition.parent, edition.edition, mint
            );
        }
        ("show", Some(arg_matches)) => {
            show(arg_matches, payer, client);
        }
        ("show_reservation_list", Some(arg_matches)) => {
            show_reservation_list(arg_matches, payer, client);
        }
        ("mint_coins", Some(arg_matches)) => {
            mint_coins(arg_matches, payer, client);
        }
        ("puff_unpuffed_metadata", Some(arg_matches)) => {
            puff_unpuffed_metadata(arg_matches, payer, client);
        }
        ("find_all_llamas", Some(arg_matches)) => {
            find_all_llamas(arg_matches, payer, client);
        }

        ("pull_llama_arweave_uris", Some(arg_matches)) => {
            pull_llama_arweave_uris(arg_matches, payer, client);
        }
        ("airdrop", Some(arg_matches)) => {
            airdrop(arg_matches, payer, client);
        }
        ("create_new_llamas", Some(arg_matches)) => {
            create_new_llamas(arg_matches, payer, client);
        }
        ("update_new_llamas", Some(arg_matches)) => {
            update_new_llamas(arg_matches, payer, client);
        }
        ("file_refunds", Some(arg_matches)) => {
            file_refund(arg_matches, payer, client);
        }

        _ => unreachable!(),
    }
}
