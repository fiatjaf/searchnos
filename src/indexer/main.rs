use chrono::{DateTime, TimeZone, Utc};
use elasticsearch::{
    http::transport::{SingleNodeConnectionPool, TransportBuilder},
    indices::IndicesPutIndexTemplateParts,
    ingest::IngestPutPipelineParts,
    DeleteByQueryParts, DeleteParts, Elasticsearch, IndexParts, SearchParts,
};
use env_logger;
use log::{error, info, warn};
use nostr_sdk::prelude::*;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    env,
};

async fn put_pipeline(
    es_client: &Elasticsearch,
    pipeline_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("putting pipeline: {}", pipeline_name);
    let res = es_client
        .ingest()
        .put_pipeline(IngestPutPipelineParts::Id(pipeline_name))
        .body(json!({
            "description": "nostr pipeline",
            "processors": [
                {
                    "inference": {
                        "model_id": "lang_ident_model_1",
                        "inference_config": {
                            "classification": {
                                "num_top_classes": 3
                            }
                        },
                        "field_mappings": {},
                        "target_field": "_ml.lang_ident"
                    }
                },
                {
                    "rename": {
                        "field": "_ml.lang_ident.predicted_value",
                        "target_field": "language"
                    }
                },
                {
                    "remove": {
                        "field": "_ml"
                    }
                },
                {
                    "set": {
                        "field": "timestamp",
                        "value": "{{{_ingest.timestamp}}}"
                    }
                }
            ]
        }))
        .send()
        .await?;

    if !res.status_code().is_success() {
        let status = res.status_code();
        let body = res.text().await?;
        return Err(format!("failed to put pipeline: received {}, {}", status, body).into());
    }
    Ok(())
}

async fn create_index_template(
    es_client: &Elasticsearch,
    template_name: &str,
    pipeline_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("putting index template: {}", template_name);
    let res = es_client
        .indices()
        .put_index_template(IndicesPutIndexTemplateParts::Name(template_name))
        .body(json!({
            "index_patterns": ["nostr-*"],
            "template": {
                "settings": {
                    "index": {
                        "number_of_shards": 1,
                        "number_of_replicas": 0,
                        "analysis": {
                            "analyzer": {
                                "ngram_analyzer": {
                                "type": "custom",
                                "tokenizer": "ngram_tokenizer",
                                "filter": ["icu_normalizer", "lowercase"],
                                },
                            },
                            "tokenizer": {
                                "ngram_tokenizer": {
                                "type": "ngram",
                                "min_gram": "1",
                                "max_gram": "2",
                                },
                            },
                        },
                        "default_pipeline": pipeline_name
                    },
                },
                "mappings": {
                    "dynamic": false,
                    "properties": {
                        "event": {
                            "dynamic": false,
                            "properties": {
                                "content": {
                                    "type": "text",
                                    "index": false
                                },
                                "created_at": {
                                    "type": "date",
                                    "format": "epoch_second"
                                },
                                "kind": {
                                    "type": "integer"
                                },
                                "id": {
                                    "type": "text",
                                    "index_prefixes": {
                                        "min_chars": 1,
                                        "max_chars": 19
                                    }
                                },
                                "pubkey": {
                                    "type": "text",
                                    "index_prefixes": {
                                        "min_chars": 1,
                                        "max_chars": 19
                                    }
                                },
                                "sig": {
                                    "type": "keyword",
                                    "index": false
                                },
                                "tags": {
                                    "type": "keyword"
                                },
                            }
                        },
                        "text": {
                            "type": "text",
                            "analyzer": "ngram_analyzer",
                            "index": "true",
                        },
                        "language": {
                            "type": "keyword"
                        },
                        "timestamp": {
                            "type": "date"
                        },
                        "tags": {
                            "dynamic": true,
                            "properties": {
                                "*": {
                                    "type": "keyword"
                                }
                            }
                        }
                    }
                },
                "aliases": {
                    "nostr": {}
                }
            }
        }))
        .send()
        .await?;

    if !res.status_code().is_success() {
        let status = res.status_code();
        let body = res.text().await?;
        return Err(format!("failed to create index: received {}, {}", status, body).into());
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct Document {
    event: Event,
    text: String,
    tags: HashMap<String, HashSet<String>>,
}

const DATE_FORMAT: &str = "%Y.%m.%d";

fn index_name_for_event(prefix: &str, event: &Event) -> Result<String, Box<dyn std::error::Error>> {
    let dt = chrono::Utc.timestamp_opt(event.created_at.as_i64(), 0);
    if let Some(dt) = dt.single() {
        Ok(format!("{}-{}", prefix, dt.format(DATE_FORMAT).to_string()))
    } else {
        Err(format!("failed to parse date: {}", event.created_at).into())
    }
}

fn can_exist(
    index_name: &str,
    current_time: &DateTime<Utc>,
    ttl_in_days: u64,
    allow_future_days: u64,
) -> Result<bool, Box<dyn std::error::Error>> {
    let date_str = index_name.split('-').nth(1).unwrap_or("");
    let index_date = chrono::NaiveDate::parse_from_str(date_str, DATE_FORMAT)?;
    let index_time = index_date.and_hms_opt(0, 0, 0);
    let index_time = if let Some(index_time) = index_time {
        index_time
    } else {
        return Ok(false);
    };
    let index_time = Utc.from_utc_datetime(&index_time);
    let ttl_duration: chrono::Duration = chrono::Duration::days(ttl_in_days as i64);
    let diff: chrono::Duration = current_time.signed_duration_since(index_time);
    Ok(-chrono::Duration::days(allow_future_days as i64) <= diff && diff < ttl_duration)
}

fn convert_tags(tags: &Vec<nostr_sdk::Tag>) -> HashMap<String, HashSet<String>> {
    let mut tag: HashMap<String, HashSet<String>> = HashMap::new();

    for t in tags {
        let t = t.as_vec();
        let mut it = t.iter();
        let tag_kind = it.next();
        let first_tag_value = it.next();
        if let (Some(tag_kind), Some(first_tag_value)) = (tag_kind, first_tag_value) {
            if tag_kind.len() != 1 {
                continue; // index only 1-char tags; See NIP-12
            }

            if let Some(values) = tag.get_mut(tag_kind) {
                values.insert(first_tag_value.clone());
            } else {
                let mut hs = HashSet::new();
                hs.insert(first_tag_value.clone());
                tag.insert(tag_kind.to_string(), hs);
            }
        }
    }

    tag
}

fn extract_text(event: &Event) -> String {
    match event.kind {
        Kind::TextNote | Kind::LongFormTextNote => event.content.clone(),
        _ => {
            let content: HashMap<String, String> =
                serde_json::from_str(&event.content).unwrap_or_default();
            let texts: Vec<String> = content.values().map(|s| s.to_string()).collect();
            texts.join(" ")
        }
    }
}

fn is_replaceable_event(event: &Event) -> bool {
    match event.kind {
        Kind::Replaceable(_) => true,
        Kind::Metadata | Kind::ContactList | Kind::ChannelMetadata => true,
        _ => false,
    }
}

async fn handle_update(
    es_client: &Elasticsearch,
    index_prefix: &str,
    alias_name: &str,
    event: &Event,
) -> Result<(), Box<dyn std::error::Error>> {
    let index_name = index_name_for_event(index_prefix, event)?;
    info!("{} {}", index_name, event.as_json());

    // TODO parameterize ttl
    let ok = can_exist(&index_name, &Utc::now(), 7, 1).unwrap_or(false);
    if !ok {
        warn!("index {} is out of range; skipping", index_name);
        return Ok(());
    }

    let id = event.id.to_hex();

    let doc = Document {
        event: event.clone(),
        text: extract_text(&event),
        tags: convert_tags(&event.tags),
    };
    let res = es_client
        .index(IndexParts::IndexId(index_name.as_str(), &id))
        .body(doc)
        .send()
        .await?;
    if !res.status_code().is_success() {
        let status_code = res.status_code();
        let body = res.text().await?;
        error!("failed to index; received {}, {}", status_code, body);
    }

    if is_replaceable_event(event) {
        let res = es_client
            .delete_by_query(DeleteByQueryParts::Index(&[alias_name]))
            .body(json!({
                "query": {
                    "bool": {
                        "must": [
                            {
                                "term": {
                                    "event.pubkey": event.pubkey.to_string()
                                }
                            },
                            {
                                "term": {
                                    "event.kind": event.kind
                                }
                            },
                            {
                                "range": {
                                    "event.created_at": {
                                        "lt": event.created_at.to_string()
                                    }
                                }
                            }
                        ]
                    }
                }
            }))
            .send()
            .await?;
        if !res.status_code().is_success() {
            let status_code = res.status_code();
            let body = res.text().await?;
            return Err(format!("failed to fetch; received {}, {}", status_code, body).into());
        }
        let response_body = res.json::<serde_json::Value>().await?;
        info!(
            "replaceable event (kind {}): deleted {} event(s) of for pubkey {}",
            event.kind.as_u32(),
            response_body["deleted"],
            event.pubkey,
        );
    }

    Ok(())
}

async fn delete_event(
    es_client: &Elasticsearch,
    alias_name: &str,
    id: &str,
    pubkey: &XOnlyPublicKey,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("try to delete: id={}", id);

    let res = es_client
        .search(SearchParts::Index(&[alias_name]))
        .body(json!({
            "query": {
                "term": {
                    "_id": id
                }
            }
        }))
        .size(1)
        .send()
        .await?;

    if !res.status_code().is_success() {
        let status_code = res.status_code();
        let body = res.text().await?;
        return Err(format!("failed to fetch; received {}, {}", status_code, body).into());
    }
    let response_body = res.json::<serde_json::Value>().await?;
    let hits_array = match response_body["hits"]["hits"].as_array() {
        Some(hits) => hits,
        None => return Err("Failed to retrieve hits from response".into()),
    };
    let hit = match hits_array.first() {
        Some(hit) => hit,
        None => return Err(format!("Event with ID {} not found in search results", id).into()),
    };

    let event_to_be_deleted = serde_json::from_value::<Event>(hit["_source"]["event"].clone())?;

    let ok_to_delete = *pubkey == event_to_be_deleted.pubkey;
    info!(
        "can event {} be deleted? {}",
        event_to_be_deleted.id, ok_to_delete
    );
    if !ok_to_delete {
        return Err(format!(
            "pubkey mismatch: pub key was {}, but that of the event {} was {}",
            pubkey, id, event_to_be_deleted.pubkey
        )
        .into());
    }

    let index_name = hit["_index"].as_str();
    let index_name = match index_name {
        Some(index_name) => index_name,
        None => return Err("failed to get index name".into()),
    };

    let res = es_client
        .delete(DeleteParts::IndexId(&index_name, &id))
        .send()
        .await?;

    if !res.status_code().is_success() {
        let status_code = res.status_code();
        let body = res.text().await?;
        error!("failed to delete; received {}, {}", status_code, body);
        return Err("failed to delete".into());
    }
    info!("deleted: id={}", id);
    Ok(())
}

async fn handle_deletion_event(
    es_client: &Elasticsearch,
    index_name: &str,
    event: &Event,
) -> Result<(), Box<dyn std::error::Error>> {
    let deletion_event = event;
    println!("deletion event: {}", deletion_event.as_json());
    for tag in &deletion_event.tags {
        if let Tag::Event(e, _, _) = tag {
            let id = e.to_hex();
            let result = delete_event(es_client, index_name, &id, &deletion_event.pubkey).await;
            if result.is_err() {
                error!("failed to delete event; {}", result.err().unwrap());
                continue;
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env::set_var("RUST_LOG", "info");
    env_logger::init();

    // env vars
    let es_url = env::var("ES_URL").expect("ES_URL is not set; set it to the URL of elasticsearch");
    let relays = env::var("NOSTR_RELAYS")
        .expect("NOSTR_RELAYS is not set; set it to the comma-separated URLs of relays");

    // prepare elasticsearch client
    let es_url = Url::parse(&es_url).expect("invalid elasticsearch url");
    let conn_pool = SingleNodeConnectionPool::new(es_url);
    let es_transport = TransportBuilder::new(conn_pool).disable_proxy().build()?;
    let es_client = Elasticsearch::new(es_transport);
    let pipeline_name = "nostr-pipeline";
    put_pipeline(&es_client, pipeline_name).await?;

    let index_template_name = "nostr";
    let alias_name = "nostr";

    create_index_template(&es_client, index_template_name, pipeline_name).await?;
    info!("elasticsearch index ready");

    // prepare nostr client
    let my_keys: Keys = Keys::generate();
    let nostr_client = Client::new(&my_keys);

    for relay in relays.split(',') {
        info!("adding relay: {}", relay);
        nostr_client.add_relay(relay, None).await?;
    }
    nostr_client.connect().await;
    info!("connected to relays");

    let subscription =
        Filter::new()
            .limit(0)
            .kinds(vec![Kind::Metadata, Kind::TextNote, Kind::EventDeletion]);

    nostr_client.subscribe(vec![subscription]).await;
    info!("ready to receive messages");

    // TODO periodically purge old indexes

    loop {
        let mut notifications = nostr_client.notifications();
        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event(_url, event) = notification {
                match event.kind {
                    Kind::Metadata | Kind::TextNote => {
                        handle_update(&es_client, &alias_name, &index_template_name, &event)
                            .await?;
                    }
                    Kind::EventDeletion => {
                        handle_deletion_event(&es_client, &index_template_name, &event).await?;
                    }
                    _ => {
                        continue;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::can_exist;

    #[test]
    fn test_can_exist() {
        let current_time = chrono::DateTime::from_str("2023-03-20T00:00:00Z").unwrap();
        assert_eq!(
            can_exist("nostr-2023.03.22", &current_time, 2, 1).unwrap(),
            false
        );
        assert_eq!(
            can_exist("nostr-2023.03.21", &current_time, 2, 1).unwrap(),
            true
        );
        assert_eq!(
            can_exist("nostr-2023.03.20", &current_time, 2, 1).unwrap(),
            true
        );
        assert_eq!(
            can_exist("nostr-2023.03.19", &current_time, 2, 1).unwrap(),
            true
        );
        assert_eq!(
            can_exist("nostr-2023.03.18", &current_time, 2, 1).unwrap(),
            false
        );
    }
}
