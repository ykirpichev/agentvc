# One always-free e2-micro VM running deploy/docker-compose.yml.
# Nothing here has been applied. See deploy/README.md before running it.
#
#   cd deploy/gcp
#   terraform init
#   terraform apply -var project=YOUR_PROJECT -var-file=secrets.tfvars

terraform {
  required_version = ">= 1.5"
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 6.0"
    }
  }
}

variable "project" {
  type        = string
  description = "GCP project id"
}

variable "region" {
  type        = string
  default     = "us-central1"
  description = "Always-free e2-micro regions: us-west1, us-central1, us-east1"
}

variable "zone" {
  type    = string
  default = "us-central1-a"
}

variable "image" {
  type    = string
  default = "ghcr.io/ykirpichev/agentvc-cloud:latest"
}

variable "env_file" {
  type        = string
  sensitive   = true
  description = "Contents of /etc/agentvc-cloud.env (see ../agentvc-cloud.env.example)"
}

variable "backup_bucket" {
  type        = string
  default     = ""
  description = "Optional GCS bucket for nightly database copies; empty keeps them on the VM only"
}

provider "google" {
  project = var.project
  region  = var.region
}

resource "google_service_account" "vm" {
  account_id   = "agentvc-cloud"
  display_name = "agentvc-cloud VM"
}

resource "google_storage_bucket_iam_member" "backups" {
  count  = var.backup_bucket == "" ? 0 : 1
  bucket = var.backup_bucket
  role   = "roles/storage.objectCreator"
  member = "serviceAccount:${google_service_account.vm.email}"
}

resource "google_compute_instance" "vm" {
  name         = "agentvc-cloud"
  machine_type = "e2-micro"
  zone         = var.zone
  tags         = ["agentvc-cloud"]

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12"
      size  = 30
      type  = "pd-standard" # the free tier covers 30 GB of standard disk
    }
  }

  network_interface {
    network = "default"
    # An ephemeral public IP for outbound traffic (image pulls, the
    # Cloudflare tunnel). No inbound port is opened: see the firewall below.
    access_config {}
  }

  service_account {
    email  = google_service_account.vm.email
    scopes = ["cloud-platform"]
  }

  metadata = {
    agentvc-env           = var.env_file
    agentvc-image         = var.image
    agentvc-backup-bucket = var.backup_bucket
    agentvc-compose       = file("${path.module}/../docker-compose.yml")
    enable-oslogin        = "TRUE"
  }
  metadata_startup_script = file("${path.module}/startup.sh")

  scheduling {
    automatic_restart   = true
    on_host_maintenance = "MIGRATE"
  }
}

# SSH only through Identity-Aware Proxy (`gcloud compute ssh --tunnel-through-iap`).
# Web traffic arrives through the Cloudflare tunnel, which dials out.
resource "google_compute_firewall" "iap_ssh" {
  name          = "agentvc-cloud-iap-ssh"
  network       = "default"
  direction     = "INGRESS"
  source_ranges = ["35.235.240.0/20"]
  target_tags   = ["agentvc-cloud"]
  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}

output "ssh" {
  value = "gcloud compute ssh agentvc-cloud --zone ${var.zone} --tunnel-through-iap"
}
