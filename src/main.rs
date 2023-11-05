#[macro_use]
extern crate rocket;
extern crate octocrab;
extern crate postgrest;
extern crate sqlite;
use octocrab::models;
use serde::{Deserialize, Serialize};

use chrono::{DateTime, Utc};
use postgrest::Postgrest;
use serde_json::json;
use std::time::Duration;

use reqwest;
use rocket::{
    tokio::{task, time},
    Request, State,
};
use rocket_dyn_templates::{context, Template};

#[derive(Serialize, Deserialize, Debug)]
struct IssueObject {
    id: i32,
    number: i32,
    created_at: String,
    title: String,
    last_processed: String,
    author: String,
}
// simple health check endpoint on /healthz
#[get("/healthz")]
fn healthz() -> &'static str {
    "OK"
}

#[get("/")]
async fn index(postgrest: &State<postgrest::Postgrest>) -> Template {
    let pg = postgrest.inner().clone();
    let resp = pg
        .from("Issues")
        .select("*")
        .execute()
        .await
        .expect("error reading from issues table")
        .text()
        .await
        .expect("error unwrapping text response");
    let parsed: Vec<IssueObject> = serde_json::from_str(&resp).expect("Failed to parse JSON");
    let res = Template::render(
        "index",
        context! {
            links: &parsed,
            title: "About"
        },
    );
    return res;
}

#[catch(500)]
fn internal_error(request: &Request) -> String {
    // Here you could inspect the request to log more details or take action.
    eprintln!("Internal Server Error: {:?}", request);
    // read the response body of the request
    format!("Sorry, '{}' is not a valid path.", request.uri())
}

#[rocket::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    octocrab::initialise(
        octocrab::Octocrab::builder()
            .personal_token(
                option_env!("GITHUB_TOKEN")
                    .ok_or("can't find github token")?
                    .to_string(),
            )
            .build()?,
    );

    let pg = Postgrest::new(
        option_env!("SUPABASE_URL")
            .ok_or("can't find supabase url")?
            .to_string(),
    )
    .insert_header(
        "apikey",
        option_env!("SUPABASE_TOKEN")
            .ok_or("error finding supabase token")?
            .to_string(),
    );

    let _forever = task::spawn(async {
        let mut interval = time::interval(Duration::from_millis(10000));
        loop {
            interval.tick().await;
            // handle the error from job_func gracefully
            if let Err(e) = job_func().await {
                log::error!("error running cron: {}", e);
            }
        }
    });

    let _rocket = rocket::build()
        .manage(pg)
        .mount("/", routes![index, healthz])
        .register("/", catchers![internal_error])
        .attach(Template::fairing())
        .launch()
        .await?;

    Ok(())
}

async fn job_func() -> Result<(), Box<dyn std::error::Error>> {
    // TODO, we shouldn't initialize supabase twice here.
    let pg2 = Postgrest::new(
        option_env!("SUPABASE_URL")
            .ok_or("error finding supabase url")?
            .to_string(),
    )
    .insert_header(
        "apikey",
        option_env!("SUPABASE_TOKEN")
            .ok_or("error finding supabase token")?
            .to_string(),
    );
    // retrieve the most recently updated record in the issues db.
    // this represents the last time we did a scan of the issues.
    let now = Utc::now();
    let res = pg2
        .from("Issues")
        .select("*")
        .not("is", "last_processed", "null")
        .order("last_processed.desc")
        .limit(1)
        .execute()
        .await?
        .text()
        .await?;
    log::info!("content of issues db: {}", res);
    let obj: Vec<IssueObject> =
        serde_json::from_str(res.as_str()).map_err(|e| format!("error parsing json: {}", e))?;
    if obj.len() != 1 {
        return Err("length of return value from supabase is not 1".into());
    }
    let last_processed_datetime: DateTime<Utc> = obj[0]
        .last_processed
        .parse()
        .map_err(|e| format!("error parsing datetime: {}", e))?;
    log::info!("last processed datetime is '{}' for issue #{} with title '{}'", last_processed_datetime, obj[0].number, obj[0].title);
    let mut issue_vec: Vec<IssueObject> = Vec::new();
    let mut page = octocrab::instance()
        .issues("highlight", "highlight")
        .list()
        .state(octocrab::params::State::Open)
        .per_page(50)
        .send()
        .await?;
    // TODO: add "jay-khatri" once we know this is working.
    let highlight_devs: Vec<&str> = vec![
        "Vadman97",
        "SpennyNDaJets",
        "deltaepsilon",
        "ccschmitz",
        "mayberryzane",
    ];
    loop {
        for issue in &page {
            if issue.created_at < last_processed_datetime {
                break;
            }

            issue_vec.push(IssueObject {
                number: issue.number.to_string().parse::<i32>()?,
                id: issue.id.to_string().parse::<i32>()?,
                created_at: issue.created_at.to_string(),
                title: issue.title.to_string(),
                last_processed: Utc::now().to_string(),
                author: issue.user.login.to_string(),
            });
        }
        page = match octocrab::instance()
            .get_page::<models::issues::Issue>(&page.next)
            .await?
        {
            Some(next_page) => next_page,
            None => break,
        }
    }

    log::info!("Found {:?} new issues to write", &issue_vec.len());

    // post in slack if we find an brand new issue
    let webhook_url =
        "https://hooks.slack.com/services/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";

    let client = reqwest::Client::new();
    // enumerate this for loop , i want an index for each entry
    for issue in &mut issue_vec.iter_mut() {
        // make sure that all issues that we processed have their "last_processed" time updated.
        issue.last_processed = now.to_string();
        // filter our issues created by a certain person.
        // convert string to str
        if highlight_devs.contains(&issue.author.as_str()) {
            log::info!("Ignoring {:?} because its author is at Highlight", issue.number);
            break;
        }
        log::info!("Not ignoring {:?} because its author is external", issue.number);

        let new_issues_message = json!({
            "text": format!("
            I noticed a new external <https://github.com/highlight/highlight/issues/{}|issue>. Take a look?", issue.number),
        });
        let res = client
            .post(webhook_url)
            .body(new_issues_message.to_string())
            .send()
            .await?;
        if res.status().is_success() {
            log::info!("sent message to slack for issue: {}", issue.number);
        } else {
            log::error!("error sending message to slack for issue: {}", issue.number);
        }
        // post on the issue to notify the the user that we've seen it.
        octocrab::instance().issues("highlight", "highlight")
                .create_comment(issue.number.try_into()?, "Hi, this is Highlight.io's [GitSorted](https://gitsorted.onrender.com/) bot. 
                While one of our developers cooks up a reply, please search for anything related in our [Discord](https://highlight.io/community) or on our [docs](https://highlight.io/docs). 
                Also, if you haven't posted a reproduction, please do so (we prioritize those tickets).")
                .await.map_err(|e| format!("error writing comment to GitHub: {}", e))?;
        log::info!("sent reply  on github for issue: {}", issue.number);
    }

    if issue_vec.len() != 0 {
        let resp = pg2.from("Issues")
            .upsert(serde_json::to_string(&issue_vec)?)
            .execute()
            .await?
            .text()
            .await?;
        log::info!("attempted writing {} issues to the db. response: \n{}", issue_vec.len(), resp.as_str())
    }
    Ok(())
}
