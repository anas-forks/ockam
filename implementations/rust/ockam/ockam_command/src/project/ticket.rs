use crate::util::duration::duration_parser;
use clap::Args;
use ockam_api::config::cli::TrustContextConfig;
use ockam_api::identity::EnrollmentTicket;
use std::collections::HashMap;
use std::time::Duration;

use miette::{miette, IntoDiagnostic};
use ockam::identity::Identifier;
use ockam::Context;
use ockam_api::authenticator::enrollment_tokens::{Members, TokenIssuer};
use ockam_api::cli_state::{CliState, StateDirTrait, StateItemTrait};
use ockam_api::config::lookup::{ProjectAuthority, ProjectLookup};
use ockam_api::nodes::InMemoryNode;

use ockam_multiaddr::{proto, MultiAddr, Protocol};

use crate::identity::{get_identity_name, initialize_identity_if_default};

use crate::util::api::{CloudOpts, TrustContextOpts};
use crate::util::node_rpc;
use crate::{docs, CommandGlobalOpts, Result};

const LONG_ABOUT: &str = include_str!("./static/ticket/long_about.txt");
const AFTER_LONG_HELP: &str = include_str!("./static/ticket/after_long_help.txt");

/// Add members to a project as an authorised enroller.
#[derive(Clone, Debug, Args)]
#[command(
    long_about = docs::about(LONG_ABOUT),
    after_long_help = docs::after_help(AFTER_LONG_HELP),
)]
pub struct TicketCommand {
    /// Orchestrator address to resolve projects present in the `at` argument
    #[command(flatten)]
    cloud_opts: CloudOpts,

    #[command(flatten)]
    trust_opts: TrustContextOpts,

    #[arg(long, short, conflicts_with = "expires_in")]
    member: Option<Identifier>,

    #[arg(long, short, default_value = "/project/default")]
    to: MultiAddr,

    /// Attributes in `key=value` format to be attached to the member
    #[arg(short, long = "attribute", value_name = "ATTRIBUTE")]
    attributes: Vec<String>,

    #[arg(long = "expires-in", value_name = "DURATION", conflicts_with = "member", value_parser=duration_parser)]
    expires_in: Option<Duration>,

    #[arg(
        long = "usage-count",
        value_name = "USAGE_COUNT",
        conflicts_with = "member"
    )]
    usage_count: Option<u64>,
}

impl TicketCommand {
    pub fn run(self, opts: CommandGlobalOpts) {
        initialize_identity_if_default(&opts, &self.cloud_opts.identity);
        node_rpc(run_impl, (opts, self));
    }

    fn attributes(&self) -> Result<HashMap<&str, &str>> {
        let mut attributes = HashMap::new();
        for attr in &self.attributes {
            let mut parts = attr.splitn(2, '=');
            let key = parts.next().ok_or(miette!("key expected"))?;
            let value = parts.next().ok_or(miette!("value expected)"))?;
            attributes.insert(key, value);
        }
        Ok(attributes)
    }
}

async fn run_impl(
    ctx: Context,
    (opts, cmd): (CommandGlobalOpts, TicketCommand),
) -> miette::Result<()> {
    let trust_context_config = cmd.trust_opts.to_config(&opts.state)?.build();
    let node = InMemoryNode::start_with_trust_context(
        &ctx,
        &opts.state,
        cmd.trust_opts.project_path.as_ref(),
        trust_context_config,
    )
    .await?;

    let mut project: Option<ProjectLookup> = None;
    let mut trust_context: Option<TrustContextConfig> = None;

    let authority_node = if let Some(tc) = cmd.trust_opts.trust_context.as_ref() {
        let tc = &opts.state.trust_contexts.read_config_from_path(tc)?;
        trust_context = Some(tc.clone());
        let cred_retr = tc
            .authority()
            .into_diagnostic()?
            .own_credential()
            .into_diagnostic()?;
        let addr = match cred_retr {
            ockam_api::config::cli::CredentialRetrieverConfig::FromCredentialIssuer(c) => {
                &c.multiaddr
            }
            _ => {
                return Err(miette!(
                    "Trust context must be configured with a credential issuer"
                ));
            }
        };
        let identity = get_identity_name(&opts.state, &cmd.cloud_opts.identity);
        let authority_identifier = tc
            .authority()
            .into_diagnostic()?
            .identity()
            .await
            .into_diagnostic()?
            .identifier()
            .clone();

        node.create_authority_client(&authority_identifier, addr, Some(identity))
            .await?
    } else if let (Some(p), Some(a)) = get_project(&opts.state, &cmd.to).await? {
        let identity = get_identity_name(&opts.state, &cmd.cloud_opts.identity);
        project = Some(p);
        node.create_authority_client(a.identity_id(), a.address(), Some(identity))
            .await?
    } else {
        return Err(miette!("Cannot create a ticket. Please specify a route to your project or to an authority node"));
    };
    // If an identity identifier is given add it as a member, otherwise
    // request an enrollment token that a future member can use to get a
    // credential.
    if let Some(id) = &cmd.member {
        authority_node
            .add_member(&ctx, id.clone(), cmd.attributes()?)
            .await?
    } else {
        let token = authority_node
            .create_token(&ctx, cmd.attributes()?, cmd.expires_in, cmd.usage_count)
            .await?;

        let ticket = EnrollmentTicket::new(token, project, trust_context);
        let ticket_serialized = ticket.hex_encoded().into_diagnostic()?;
        opts.terminal
            .clone()
            .stdout()
            .machine(ticket_serialized)
            .write_line()?;
    }

    Ok(())
}

/// Get the project authority from the first address protocol.
///
/// If the first protocol is a `/project`, look up the project's config.
async fn get_project(
    cli_state: &CliState,
    input: &MultiAddr,
) -> Result<(Option<ProjectLookup>, Option<ProjectAuthority>)> {
    if let Some(proto) = input.first() {
        if proto.code() == proto::Project::CODE {
            let proj = proto.cast::<proto::Project>().expect("project protocol");
            return if let Ok(p) = cli_state.projects.get(proj.to_string()) {
                let c = p.config();
                let a =
                    ProjectAuthority::from_raw(&c.authority_access_route, &c.authority_identity)
                        .await?;
                if a.is_some() {
                    let p = ProjectLookup::from_project(c).await?;
                    Ok((Some(p), a))
                } else {
                    Err(miette!("missing authority in project {:?}", &*proj).into())
                }
            } else {
                Err(miette!("unknown project {}", &*proj).into())
            };
        }
    }
    Ok((None, None))
}
