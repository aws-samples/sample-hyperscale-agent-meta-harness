# Sources for architecture.svg and architecture.png

Text version of the picture: the section "How it works in 8 steps" of `README.md`.

## Icon package

| Item | Value |
| --- | --- |
| Page | https://aws.amazon.com/architecture/icons/ |
| Package name | `Icon-package_07312026.zip` (the page calls it "Icon package") |
| Package URL | https://d1.awsstatic.com/onedam/marketing-channels/website/public/shared/architecture-icon-release/Icon-package_07312026.5846e92413caa21490223536cc97f1269e44fa92.zip |
| Release | `24-2026.07.31`, the release on the title slide of the AWS deck in the PPTx toolkit below. The packages ship quarterly (end of January, April and July). |
| Size | 13,988,918 bytes |
| sha256 | `d2d166c453526471749d520e0db022c459abef759d2946cf2dd1d1c992dc6526` |
| Guidelines source | The AWS deck in `Microsoft-PPTx-toolkits_07312026.zip` (URL on the same page: https://d1.awsstatic.com/onedam/marketing-channels/website/public/shared/architecture-icon-release/Microsoft-PPTx-toolkits_07312026.1c286c4a809a3cf2902c88ff63bf7dd1fa3cd55d.zip, sha256 `49b19b5cc99e813453be67fc2389a8abce115d7a3261c75698e62cefe4918221`). |

The package is untrusted data. Extraction lists the zip first, writes only `.svg` and `.png` members into a new empty directory, skips __MACOSX/ and `.DS_Store` entries, and refuses unsafe names, symlinks and SVGs that contain `<script`. The package holds 4,035 icon files. No icon file is modified: each is embedded byte for byte as a base64 data URI.

## Icons used (19 files: 13 service icons, 2 resource icons (Client and STS), 4 group icons)

| Key | File in the package | sha256 |
| --- | --- | --- |
| g_cloud | Architecture-Group-Icons_07312026/AWS-Cloud-logo_32.svg | `63d73df1aac59dab323df732f82e196e359fb7b6370c0ca8e51e7b897cd350be` |
| g_private | Architecture-Group-Icons_07312026/Private-subnet_32.svg | `0666b2c8784d303a850c7cd1b40236f3d6d97012b4f4d2bad65a28c6ec614ba4` |
| g_region | Architecture-Group-Icons_07312026/Region_32.svg | `848203d40c55e213c84c1b8a3e14c2fa92aaa793b31a5c17a5fd1737ef58b8ca` |
| g_vpc | Architecture-Group-Icons_07312026/Virtual-private-cloud-VPC_32.svg | `a225286901ea59f74d52c37ccaa2660f9e2728a76cd4ee0892c29c7a386ef98c` |
| sfn | Architecture-Service-Icons_07312026/Arch_Application-Integration/48/Arch_AWS-Step-Functions_48.svg | `c1bb81d6504d283c6b31cbd9516fa6b8364010ea3c30ae614507114ca34192ad` |
| agentcore | Architecture-Service-Icons_07312026/Arch_Artificial-Intelligence/48/Arch_Amazon-Bedrock-AgentCore_48.svg | `c715fe6dbc4b661c3cb9b57f8d2ea2176218cdf57b8298901c36c808d96b52e8` |
| bedrock | Architecture-Service-Icons_07312026/Arch_Artificial-Intelligence/48/Arch_Amazon-Bedrock_48.svg | `01c39972d595d848ffc91d31aa6087b033c65279f8416c7c9a6659bc54406182` |
| lambda | Architecture-Service-Icons_07312026/Arch_Compute/48/Arch_AWS-Lambda_48.svg | `4a376abd2d67f8916791458810f3640b12fcfa71079154e0aed0974b92e58cdb` |
| ecr | Architecture-Service-Icons_07312026/Arch_Containers/48/Arch_Amazon-Elastic-Container-Registry_48.svg | `bf829eab882165f4fb850bab032e5c81312cb579e2ad880372f206936db4a2be` |
| dynamodb | Architecture-Service-Icons_07312026/Arch_Databases/48/Arch_Amazon-DynamoDB_48.svg | `c336f941abcb25012c6e36fd884123a868d0727cf916d6f2348b0fb0a053f2f0` |
| codeartifact | Architecture-Service-Icons_07312026/Arch_Developer-Tools/48/Arch_AWS-CodeArtifact_48.svg | `b569ee504727e096ed22d147de3920f8c70b3478d912728fd562364a03a894b5` |
| codecommit | Architecture-Service-Icons_07312026/Arch_Developer-Tools/48/Arch_AWS-CodeCommit_48.svg | `9257c70eb2067fffbd4673acb6228a345b4a0340c047c2fc50c4374b722f285a` |
| cloudwatch | Architecture-Service-Icons_07312026/Arch_Management-Tools/48/Arch_Amazon-CloudWatch_48.svg | `9920f2db47fed7e2437c329e8b231edcb0f55db82f02413afa3f2d546c30d552` |
| apigw | Architecture-Service-Icons_07312026/Arch_Networking-Content-Delivery/48/Arch_Amazon-API-Gateway_48.svg | `08f0cb8f93f8d390425d3925054004c17422cabb7eee14edbabe238e678278af` |
| iam | Architecture-Service-Icons_07312026/Arch_Security-Identity/48/Arch_AWS-Identity-and-Access-Management_48.svg | `81954dcfb86d5f410f7084345e459cd5c7ca1d76032d34f771ad379c94e062b6` |
| netfw | Architecture-Service-Icons_07312026/Arch_Security-Identity/48/Arch_AWS-Network-Firewall_48.svg | `8880494187ffd79dc6b65c4c657b8ed84d043556b27d4b5eb41cb85867b01cae` |
| s3 | Architecture-Service-Icons_07312026/Arch_Storage/48/Arch_Amazon-Simple-Storage-Service_48.svg | `bb0df5b7ca52da7323888ed588baefa1c68a23b5776f325bc8ec29825c185a31` |
| client | Resource-Icons_07312026/Res_General-Icons/Res_48_Light/Res_Client_48_Light.svg | `5c0eb210ca9cca0f53c136eb524273ce5ad90e40545752aeadb129bdcb2af0f3` |
| sts | Resource-Icons_07312026/Res_Security-Identity/Res_AWS-Identity-Access-Management_AWS-STS_48.svg | `2cb69b0aba7a9c1e0a24e9213bba31841d9302af532c1a3945dbf1514a6c0154` |

Official names on the labels: Amazon API Gateway, AWS Lambda, AWS Step Functions, Amazon Bedrock AgentCore, Amazon Bedrock, AWS CodeArtifact, AWS Network Firewall, AWS CodeCommit, Amazon S3, Amazon DynamoDB, AWS IAM, AWS STS, Amazon ECR, Amazon CloudWatch. The picture uses short forms for five of them; the README spells out the full names, as slide 17 of the deck asks.

## Guidelines followed (deck slide numbers, release 24-2026.07.31)

- Slide 15: icons are used at their predefined files, never cropped, flipped, rotated or recolored. Service icons are the `_48` files drawn at 48 units, group icons the `_32` files at 32 units, resource icons (Client, STS) the `_48` files at 48 units.
- Slide 25: the AWS Cloud group uses the variant with the AWS logo (`AWS-Cloud-logo_32.svg`). Group border colours are the colours of the group icons (AWS Cloud #232F3E, Region #00A4A6 dashed, VPC #8C4FFF, private subnet #00A4A6 with a light teal fill).
- Slide 17: service names fit on two lines or fewer, no mid-word break, short forms explained in the README.
- Slide 18: numbered callouts are black circles with bold white digits, numbered left to right and top to bottom, no letters, one size.
- Slide 16: arrows are the open-arrow style with straight lines and right angles.

Deviations from the deck: label text is 13 to 14 px (bold for service names) where the deck gives 12 pt Arial regular, so it stays readable when the picture is shown 700 px wide; the picture is one SVG whose units are not points. The boxes for agentd, "Cline or OpenCode", "Other harnesses", the package proxy, the Lambda function chips and "Your Mac", and the heading note, are plain generic shapes, not AWS icons.

## What the picture shows, and where each fact comes from

The facts come from these files: `infra/lib/stacks/{network,security,data,packages,control,runtimes,orchestration-direct,delivery,observability}.ts`, `statemachines/direct.asl.json`, `statemachines/direct-run-ended.asl.json`, and the README sections "How it works in 8 steps" and "Harnesses and engines".

- API Gateway: regional REST API `hamh-direct-v1`, one `ANY /v1/{proxy+}` route, `AWS_IAM` authorization, resource policy limited to the stage account. A second, private REST API (`/internal`, reachable only through the VPC execute-api endpoint) serves worker-api.
- Lambda functions drawn (6): control-api, worker-api (control stack), run-session (orchestration), repo-gateway, bundle-validator, packager (delivery, one container image). The stage has eight Lambda functions of the product: research-api and research-job, the functions of the research route, are not drawn. CDK custom resources add helper functions that carry no name of the product (a synthesized stage holds one in the network stack and one in the runtimes stack).
- Step Functions: `hamh-direct-run` (20 states) and `hamh-direct-run-ended` (started by an EventBridge rule when a run ends ABORTED, FAILED or TIMED_OUT).
- AgentCore: one runtime in VPC mode on the private subnets, session storage mounted at /mnt/workspace, images from ECR.
- Network: VPC 10.50.0.0/16, private, public and firewall subnets, NAT, AWS Network Firewall with a strict-order domain allowlist (Bedrock endpoints of the other Regions, Mantle endpoints, api.osv.dev), interface endpoints for Bedrock, ECR, CodeArtifact, logs and execute-api, and an S3 gateway endpoint.
- The picture leaves out the four CloudWatch alarms on the stored Tavily key of a stage that uses Tavily, KMS keys, EventBridge and SSM, to save icons.
- Cline and OpenCode are the engines that run; the box "Cline or OpenCode" stands for the engine of the run. The dashed box "Other harnesses (Pi, etc.)" and the legend entry "design only" mark the engine adapter, which is a design and not a running component.

## Files

| File | Bytes | sha256 |
| --- | --- | --- |
| `architecture.svg` (1100 x 864 viewBox, `<title>` and `<desc>` inside) | 95139 | `bb7e812049e685acf9e0e0edf985c0ae7351b8cd087317bf0a79b9e3113b0124` |
| `architecture.png` (1600 x 1257) | 387618 | `59c12eb1644d00119deac98ee57e7b70b9197bc5a539899a7446d34596a823e0` |

The SVG uses only plain shapes, text, `<image>` data URIs and markers, with no external file. The PNG is the fallback for viewers that do not draw the SVG.
